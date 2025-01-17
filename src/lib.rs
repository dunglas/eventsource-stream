//! A basic building block for building an Eventsource from a Stream of bytes array like objects. To
//! learn more about Server Sent Events (SSE) take a look at [the MDN
//! docs](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events/Using_server-sent_events)
//!
//! # Example
//!
//! ```ignore
//! let mut stream = reqwest::Client::new()
//!     .get("http://localhost:7020/notifications")
//!     .send()
//!     .await?
//!     .bytes_stream()
//!     .eventsource();
//!
//!
//! while let Some(event) = stream.next().await {
//!     match event {
//!         Ok(event) => println!(
//!             "received: {:?}: {}",
//!             event.event,
//!             String::from_utf8_lossy(&event.data)
//!         ),
//!         Err(e) => eprintln!("error occured: {}", e),
//!     }
//! }
//! ```

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[macro_use]
extern crate failure;

#[cfg(not(feature = "std"))]
use alloc::{collections::VecDeque, string::String, vec::Vec};
#[cfg(feature = "std")]
use std::collections::VecDeque;

use core::pin::Pin;
use core::time::Duration;
use core::{fmt, mem};
use futures_core::stream::Stream;
use futures_core::task::{Context, Poll};

/// An Event
#[derive(Default, Debug, Eq, PartialEq)]
pub struct Event {
    /// The event name if given
    pub event: Option<String>,
    /// The event data
    pub data: Vec<u8>,
    /// The event id if given
    pub id: Option<Vec<u8>>,
    /// Retry duration if given
    pub retry: Option<Duration>,
}

#[derive(Debug, Clone, Copy)]
enum Field {
    Event,
    Data,
    Id,
    Retry,
}

#[derive(Debug, Copy, Clone)]
enum State {
    ReadingField,
    ReadingValue,
    NextLine,
    Closed,
}

/// Wrapper for [`ParseError`] and other Transport Errors thrown while collecting the
/// [`Event`] stream
#[derive(Debug)]
pub enum Error<T> {
    /// Parse Error
    Parse(ParseError),
    /// Underlying transport error
    Transport(T),
}

/// Error thrown while parsing an event line
#[derive(Debug, Fail)]
pub enum ParseError {
    /// Field name parsing error. Field must be one of `event`, `data`, `id` or `retry`. Contains
    /// buffer of attempted parsed bytes
    #[fail(display = "invalid field name: {:?}", _0)]
    InvalidField(Vec<u8>),
    /// Utf8 [`Event::event`] field parsing error. Contains buffer of attempted parsed bytes
    #[fail(display = "invalid event string: {:?}", _0)]
    InvalidEvent(Vec<u8>),
    /// Utf8 number [`Event::retry`] field parsing error. Contains buffer of attempted parsed bytes
    #[fail(display = "invalid retry duration: {:?}", _0)]
    InvalidRetry(Vec<u8>),
    /// Came to end of line without reading field. Contains buffer of attempted parsed bytes
    #[fail(display = "unexpected end of line: {:?}", _0)]
    UnexpectedEndOfLine(Vec<u8>),
    /// No field found on line
    #[fail(display = "empty field")]
    EmptyField,
}

/// Main entrypoint for creating [`Event`] streams
pub trait Eventsource: Sized {
    /// Create an event stream from a stream of bytes
    fn eventsource(self) -> EventStreamTransformer<Self>;
}

impl Field {
    fn from_bytes(bytes: Vec<u8>) -> Result<Field, ParseError> {
        let string =
            String::from_utf8(bytes).map_err(|e| ParseError::InvalidField(e.into_bytes()))?;
        if string.is_empty() {
            return Err(ParseError::EmptyField);
        }
        Ok(match string.as_ref() {
            "event" => Field::Event,
            "data" => Field::Data,
            "id" => Field::Id,
            "retry" => Field::Retry,
            _ => return Err(ParseError::InvalidField(string.into_bytes())),
        })
    }
}

impl Event {
    /// Check if an event is the default empty event
    pub fn is_empty(&self) -> bool {
        self == &Event::default()
    }

    fn set_field(&mut self, field: Option<Field>, mut data: Vec<u8>) -> Result<(), ParseError> {
        match field {
            Some(Field::Event) => {
                let event = String::from_utf8(data)
                    .map_err(|e| ParseError::InvalidEvent(e.into_bytes()))?;
                self.event = Some(event);
            }
            Some(Field::Data) => {
                self.data.append(&mut data);
            }
            Some(Field::Id) => {
                self.id = Some(data);
            }
            Some(Field::Retry) => {
                let ms = String::from_utf8(data)
                    .map_err(|e| ParseError::InvalidRetry(e.into_bytes()))
                    .and_then(|string| {
                        string
                            .parse()
                            .map_err(|_| ParseError::InvalidRetry(string.into_bytes()))
                    })?;
                self.retry = Some(Duration::from_millis(ms));
            }
            None => {}
        };
        Ok(())
    }
}

/// Provides the [`Stream`] implementation for Events
pub struct EventStreamTransformer<S> {
    buffer: Vec<u8>,
    field: Option<Field>,
    event: Event,
    state: State,
    results: VecDeque<Result<Event, ParseError>>,
    stream: S,
}

struct EventStreamTransformerProjection<'a, S> {
    buffer: &'a mut Vec<u8>,
    field: &'a mut Option<Field>,
    event: &'a mut Event,
    state: &'a mut State,
    results: &'a mut VecDeque<Result<Event, ParseError>>,
    stream: &'a mut S,
}

impl<S> EventStreamTransformer<S> {
    /// Wrap a stream of bytes
    pub fn wrap(stream: S) -> Self {
        Self {
            buffer: Vec::default(),
            field: None,
            event: Event::default(),
            state: State::ReadingField,
            results: Default::default(),
            stream,
        }
    }

    #[inline]
    fn projection<'a>(self: Pin<&'a mut Self>) -> EventStreamTransformerProjection<'a, S> {
        unsafe {
            let inner = self.get_unchecked_mut();
            EventStreamTransformerProjection {
                buffer: &mut inner.buffer,
                field: &mut inner.field,
                event: &mut inner.event,
                state: &mut inner.state,
                results: &mut inner.results,
                stream: &mut inner.stream,
            }
        }
    }
}

impl<S, B, E> Stream for EventStreamTransformer<S>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    type Item = Result<Event, Error<E>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = self.projection();

        if let Some(res) = this.results.pop_back() {
            return Poll::Ready(Some(res.map_err(Error::Parse)));
        }
        if matches!(this.state, State::Closed) {
            return Poll::Ready(None);
        }

        loop {
            let stream = unsafe { Pin::new_unchecked(&mut *this.stream) };
            let chunk = match stream.poll_next(cx) {
                Poll::Pending => {
                    if let Some(res) = this.results.pop_back() {
                        return Poll::Ready(Some(res.map_err(Error::Parse)));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(None) => {
                    if !this.event.is_empty() {
                        this.results.push_front(Ok(mem::take(this.event)));
                    }
                    *this.state = State::Closed;
                    if let Some(res) = this.results.pop_back() {
                        return Poll::Ready(Some(res.map_err(Error::Parse)));
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(Error::Transport(e))));
                }
                Poll::Ready(Some(Ok(chunk))) => chunk,
            };

            for byte in chunk.as_ref() {
                match byte {
                    b'\n' => match this.state {
                        State::ReadingField => {
                            if !this.buffer.is_empty() {
                                this.results.push_front(Err(ParseError::UnexpectedEndOfLine(
                                    mem::take(this.buffer),
                                )));
                                this.buffer.clear();
                            }
                            *this.field = None;
                            *this.state = State::NextLine;
                        }
                        State::ReadingValue => {
                            if let Err(e) = this
                                .event
                                .set_field(mem::take(this.field), mem::take(this.buffer))
                            {
                                this.results.push_front(Err(e));
                            }
                            *this.state = State::NextLine;
                        }
                        State::NextLine => {
                            this.results.push_front(Ok(mem::take(this.event)));
                            this.buffer.clear();
                            *this.field = None;
                            *this.state = State::ReadingField;
                        }
                        State::Closed => unreachable!(),
                    },
                    b':' => match this.state {
                        State::ReadingField => {
                            match Field::from_bytes(mem::take(this.buffer)) {
                                Ok(next_field) => {
                                    *this.field = Some(next_field);
                                }
                                Err(e) => {
                                    this.results.push_front(Err(e));
                                }
                            }
                            *this.state = State::ReadingValue;
                        }
                        State::ReadingValue => {
                            this.buffer.push(*byte);
                        }
                        State::NextLine => {
                            this.results.push_front(Err(ParseError::EmptyField));
                            *this.state = State::ReadingValue;
                        }
                        State::Closed => unreachable!(),
                    },
                    byte => {
                        if matches!(this.state, State::NextLine) {
                            *this.state = State::ReadingField;
                        }
                        this.buffer.push(*byte);
                    }
                }
            }
        }
    }
}

impl<S, B, E> Eventsource for S
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    fn eventsource(self) -> EventStreamTransformer<Self> {
        EventStreamTransformer::wrap(self)
    }
}

impl<T> fmt::Display for Error<T>
where
    T: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(err) => f.write_fmt(format_args!("parse error: {}", err)),
            Self::Transport(err) => f.write_fmt(format_args!("transport error: {}", err)),
        }
    }
}

#[cfg(feature = "std")]
impl<T> std::error::Error for Error<T> where T: fmt::Display + fmt::Debug + Send + Sync {}
