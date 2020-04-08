//! Complete responses.

use fnv::FnvHashSet;

use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt;
use std::iter::FusedIterator;
use std::sync::Arc;

use crate::parser;

/// Response to a command, consisting of an abitrary amount of frames, which are responses to
/// individual commands, and optionally a single error.
///
/// Since an error terminates a command list, there can only be one error in a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// The sucessful responses.
    frames: Vec<Frame>,
    /// The error, if one occured.
    error: Option<Error>,
}

/// Data in a succesful response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Key-value pairs. Keys can repeat arbitrarily often.
    pub values: Vec<(Arc<str>, String)>,
    /// Binary frame.
    pub binary: Option<Vec<u8>>,
}

/// Data in an error.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Error {
    /// Error code. See [the MPD source][mpd-error-def] for a list of of possible values.
    ///
    /// [mpd-error-def]: https://github.com/MusicPlayerDaemon/MPD/blob/master/src/protocol/Ack.hxx#L30
    pub code: u64,
    /// Index of command in a command list that caused this error. 0 when not in a command list.
    pub command_index: u64,
    /// Command that returned the error, if applicable.
    pub current_command: Option<String>,
    /// Message describing the error.
    pub message: String,
}

/// Errors returned when attmepting to construct an owned `Response` from a list of parser results
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnedResponseError {
    /// There were further frames after an error frame
    FramesAfterError,
    /// An empty slice was provided (A response needs at least one frame or error)
    Empty,
}

#[allow(clippy::len_without_is_empty)]
impl Response {
    /// Construct a new response.
    ///
    /// ```
    /// use mpd_protocol::response::{Response, Frame};
    ///
    /// let r = Response::new(vec![Frame::empty()], None);
    /// assert_eq!(1, r.len());
    /// assert!(r.is_success());
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if it is attempted to construct an empty response (i.e. both `frames` and `error`
    /// are empty). This should not occur during normal operation.
    ///
    /// ```should_panic
    /// use mpd_protocol::response::Response;
    ///
    /// // This panics:
    /// Response::new(Vec::new(), None);
    /// ```
    pub fn new(mut frames: Vec<Frame>, error: Option<Error>) -> Self {
        assert!(
            !frames.is_empty() || error.is_some(),
            "attempted to construct an empty (no frames or error) response"
        );

        frames.reverse(); // We want the frames in reverse-chronological order (i.e. oldest last).
        Self { frames, error }
    }

    /// Construct a new "empty" response. This is the simplest possible succesful response,
    /// consisting of a single empty frame.
    ///
    /// ```
    /// use mpd_protocol::response::Response;
    ///
    /// let r = Response::empty();
    /// assert_eq!(1, r.len());
    /// assert!(r.is_success());
    /// ```
    pub fn empty() -> Self {
        Self::new(vec![Frame::empty()], None)
    }

    /// Returns `true` if the response resulted in an error.
    ///
    /// Even if this returns `true`, there may still be succesful frames in the response when the
    /// response is to a command list.
    ///
    /// ```
    /// use mpd_protocol::response::{Response, Error};
    ///
    /// let r = Response::new(Vec::new(), Some(Error::default()));
    /// assert!(r.is_error());
    /// ```
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }

    /// Returns `true` if the response was entirely succesful (i.e. no errors).
    ///
    /// ```
    /// use mpd_protocol::response::{Response, Frame};
    ///
    /// let r = Response::new(vec![Frame::empty()], None);
    /// assert!(r.is_success());
    /// ```
    pub fn is_success(&self) -> bool {
        !self.is_error()
    }

    /// Get the number of succesful frames in the response.
    ///
    /// May be 0 if the response only consists of an error.
    ///
    /// ```
    /// use mpd_protocol::response::Response;
    ///
    /// let r = Response::empty();
    /// assert_eq!(r.len(), 1);
    /// ```
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Create an iterator over references to the frames in the response.
    ///
    /// ```
    /// use mpd_protocol::response::{Frame, Response};
    ///
    /// let r = Response::empty();
    /// let mut iter = r.frames();
    ///
    /// assert_eq!(Some(Ok(&Frame::empty())), iter.next());
    /// ```
    pub fn frames(&self) -> FramesRef<'_> {
        FramesRef {
            response: self,
            frames_cursor: 0,
            error_consumed: false,
        }
    }

    /// Treat the response as consisting of a single frame or error.
    ///
    /// Frames or errors beyond the first, if they exist, are silently discarded.
    ///
    /// ```
    /// use mpd_protocol::response::{Frame, Response};
    ///
    /// let r = Response::empty();
    /// assert_eq!(Ok(Frame::empty()), r.single_frame());
    /// ```
    pub fn single_frame(self) -> Result<Frame, Error> {
        // There is always at least one frame
        self.into_frames().next().unwrap()
    }

    /// Creates an iterator over all frames and errors in the response.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use mpd_protocol::response::{Frame, Response};
    ///
    /// let mut first = vec![(Arc::from("hello"), String::from("world"))];
    ///
    /// let second = vec![(Arc::from("foo"), String::from("bar"))];
    ///
    /// let mut iter = Response::new(vec![Frame {
    ///     values: first.clone(),
    ///     binary: None,
    /// }, Frame {
    ///     values: second.clone(),
    ///     binary: None,
    /// }], None).into_frames();
    ///
    /// assert_eq!(Some(Ok(Frame {
    ///     values: first,
    ///     binary: None,
    /// })), iter.next());
    ///
    /// assert_eq!(Some(Ok(Frame {
    ///     values: second,
    ///     binary: None,
    /// })), iter.next());
    ///
    /// assert_eq!(None, iter.next());
    /// ```
    pub fn into_frames(self) -> Frames {
        Frames(self)
    }
}

impl<'a> TryFrom<&'a [parser::Response<'_>]> for Response {
    type Error = OwnedResponseError;

    fn try_from(raw_frames: &'a [parser::Response<'_>]) -> Result<Self, Self::Error> {
        if raw_frames.is_empty() {
            return Err(OwnedResponseError::Empty);
        }

        // Optimistically pre-allocated Vec
        let mut frames = Vec::with_capacity(raw_frames.len());
        let mut error = None;

        let mut keys = FnvHashSet::default();

        for frame in raw_frames.iter().rev() {
            match frame {
                parser::Response::Success { fields, binary } => {
                    let values = fields
                        .iter()
                        .map(|&(k, v)| (simple_intern(&mut keys, k), v.to_owned()))
                        .collect();

                    let binary = binary.map(Vec::from);

                    frames.push(Frame { values, binary });
                }
                parser::Response::Error {
                    code,
                    command_index,
                    current_command,
                    message,
                } => {
                    if !frames.is_empty() {
                        // If we already saw succesful frames, the error would not have been the
                        // final element
                        return Err(OwnedResponseError::FramesAfterError);
                    }

                    error = Some(Error {
                        code: *code,
                        command_index: *command_index,
                        current_command: current_command.map(String::from),
                        message: (*message).to_owned(),
                    });
                }
            }
        }

        Ok(Response { frames, error })
    }
}

fn simple_intern(store: &mut FnvHashSet<Arc<str>>, value: &str) -> Arc<str> {
    match store.get(value) {
        Some(v) => Arc::clone(v),
        None => {
            let v = Arc::from(value);
            store.insert(Arc::clone(&v));
            v
        }
    }
}

/// Iterator over frames in a response, as returned by [`frames()`].
///
/// [`frames()`]: struct.Response.html#method.frames
#[derive(Copy, Clone, Debug)]
pub struct FramesRef<'a> {
    response: &'a Response,
    frames_cursor: usize,
    error_consumed: bool,
}

impl<'a> Iterator for FramesRef<'a> {
    type Item = Result<&'a Frame, &'a Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.frames_cursor < self.response.frames.len() {
            let frame = self.response.frames.get(self.frames_cursor).unwrap();
            self.frames_cursor += 1;
            Some(Ok(frame))
        } else if !self.error_consumed {
            self.error_consumed = true;
            self.response.error.as_ref().map(Err)
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let mut len = self.response.frames.len() - self.frames_cursor;

        if !self.error_consumed && self.response.is_error() {
            len += 1;
        }

        (len, Some(len))
    }
}

impl<'a> FusedIterator for FramesRef<'a> {}
impl<'a> ExactSizeIterator for FramesRef<'a> {}

impl<'a> IntoIterator for &'a Response {
    type Item = Result<&'a Frame, &'a Error>;
    type IntoIter = FramesRef<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.frames()
    }
}

/// Iterator over frames in a response, as returned by [`into_frames()`].
///
/// [`into_frames()`]: struct.Response.html#method.into_frames
#[derive(Clone, Debug)]
pub struct Frames(Response);

impl Iterator for Frames {
    type Item = Result<Frame, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(frame) = self.0.frames.pop() {
            Some(Ok(frame))
        } else if let Some(error) = self.0.error.take() {
            Some(Err(error))
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // .len() returns the number of succesful frames, add 1 if there is also an error
        let len = self.0.len() + if self.0.is_error() { 1 } else { 0 };

        (len, Some(len))
    }
}

impl FusedIterator for Frames {}
impl ExactSizeIterator for Frames {}

impl IntoIterator for Response {
    type Item = Result<Frame, Error>;
    type IntoIter = Frames;

    fn into_iter(self) -> Self::IntoIter {
        self.into_frames()
    }
}

impl Frame {
    /// Create an empty frame (0 key-value pairs).
    ///
    /// ```
    /// use mpd_protocol::response::Frame;
    ///
    /// let f = Frame::empty();
    /// assert_eq!(0, f.values.len());
    /// assert!(f.binary.is_none());
    /// ```
    pub fn empty() -> Self {
        Self {
            values: Vec::new(),
            binary: None,
        }
    }

    /// Find the first key-value pair with the given key, and return a reference to its value.
    pub fn find<K>(&self, key: K) -> Option<&str>
    where
        K: AsRef<str>,
    {
        self.values
            .iter()
            .find(|&(k, _)| k.as_ref() == key.as_ref())
            .map(|(_, v)| v.as_str())
    }

    /// Find the first key-value pair with the given key, and return its value.
    ///
    /// This removes it from the list of values in this frame.
    pub fn get<K>(&mut self, key: K) -> Option<String>
    where
        K: AsRef<str>,
    {
        let index = self
            .values
            .iter()
            .enumerate()
            .find(|&(_, (k, _))| k.as_ref() == key.as_ref())
            .map(|(index, _)| index);

        index.map(|i| self.values.remove(i).1)
    }

    /// Collect the key-value pairs in this resposne into a `HashMap`.
    ///
    /// Beware that this loses the order relationship between different keys. Values for a given
    /// key are ordered like they appear in the response.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use mpd_protocol::response::Frame;
    ///
    /// let f = Frame {
    ///     values: vec![
    ///         (Arc::from("foo"), String::from("bar")),
    ///         (Arc::from("hello"), String::from("world")),
    ///         (Arc::from("foo"), String::from("baz")),
    ///     ],
    ///     binary: None,
    /// };
    ///
    /// let map = f.values_as_map();
    ///
    /// assert_eq!(map.get("foo"), Some(&vec!["bar", "baz"]));
    /// assert_eq!(map.get("hello"), Some(&vec!["world"]));
    /// ```
    pub fn values_as_map(&self) -> HashMap<Arc<str>, Vec<&str>> {
        let mut map = HashMap::new();

        for (k, v) in self.values.iter() {
            map.entry(Arc::clone(k))
                .or_insert_with(Vec::new)
                .push(v.as_str());
        }

        map
    }
}

impl fmt::Display for OwnedResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OwnedResponseError::FramesAfterError => {
                write!(f, "Error frame was not the final element of response")
            }
            OwnedResponseError::Empty => {
                write!(f, "Attempted to construct response with no values")
            }
        }
    }
}

impl std::error::Error for OwnedResponseError {}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn owned_frames_iter() {
        let r = Response::new(
            vec![Frame::empty(), Frame::empty(), Frame::empty()],
            Some(Error::default()),
        );

        let mut iter = r.into_frames();

        assert_eq!((4, Some(4)), iter.size_hint());
        assert_eq!(Some(Ok(Frame::empty())), iter.next());

        assert_eq!((3, Some(3)), iter.size_hint());
        assert_eq!(Some(Ok(Frame::empty())), iter.next());

        assert_eq!((2, Some(2)), iter.size_hint());
        assert_eq!(Some(Ok(Frame::empty())), iter.next());

        assert_eq!((1, Some(1)), iter.size_hint());
        assert_eq!(Some(Err(Error::default())), iter.next());

        assert_eq!((0, Some(0)), iter.size_hint());
    }

    #[test]
    fn borrowed_frames_iter() {
        let r = Response::new(
            vec![Frame::empty(), Frame::empty(), Frame::empty()],
            Some(Error::default()),
        );

        let mut iter = r.frames();

        assert_eq!((4, Some(4)), iter.size_hint());
        assert_eq!(Some(Ok(&Frame::empty())), iter.next());

        assert_eq!((3, Some(3)), iter.size_hint());
        assert_eq!(Some(Ok(&Frame::empty())), iter.next());

        assert_eq!((2, Some(2)), iter.size_hint());
        assert_eq!(Some(Ok(&Frame::empty())), iter.next());

        assert_eq!((1, Some(1)), iter.size_hint());
        assert_eq!(Some(Err(&Error::default())), iter.next());

        assert_eq!((0, Some(0)), iter.size_hint());
    }
}
