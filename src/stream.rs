use std::{
    fmt,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use async_channel::{Receiver, Sender, TrySendError};
use futures_core::Stream;

use crate::address::{validate_category, validate_method};
use crate::connection::{Cancel, STREAM_CAPACITY};
use crate::{Error, ErrorKind};

/// An ordered stream of replies to a Luna method call.
#[must_use = "dropping the subscription cancels the call"]
pub struct Subscription<T = String> {
    feed: Feed<String>,
    first: Option<String>,
    cancel: Cancel,
    decode: fn(String) -> Result<T, Error>,
}

impl<T> Subscription<T> {
    pub(crate) fn new(
        feed: Feed<String>,
        first: String,
        cancel: Cancel,
        decode: fn(String) -> Result<T, Error>,
    ) -> Self {
        Self {
            feed,
            first: Some(first),
            cancel,
            decode,
        }
    }

    /// Ends this subscription and waits for the cancellation frame to be written.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection closes or the cancellation cannot be sent.
    pub async fn cancel(self) -> Result<(), Error> {
        self.cancel.wait().await
    }
}

impl<T> Stream for Subscription<T> {
    type Item = Result<T, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(first) = self.first.take() {
            return Poll::Ready(Some((self.decode)(first)));
        }
        self.feed
            .poll_next(cx)
            .map(|item| item.map(|result| result.and_then(self.decode)))
    }
}

impl<T> fmt::Debug for Subscription<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscription").finish_non_exhaustive()
    }
}

/// The category and optional method of a Luna broadcast signal.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SignalFilter {
    category: String,
    method: Option<String>,
}

impl SignalFilter {
    /// Matches every signal in the specified category.
    ///
    /// # Errors
    ///
    /// Returns an error if the category is invalid.
    pub fn category(category: &str) -> Result<Self, Error> {
        validate_category(category)?;
        Ok(Self {
            category: category.to_owned(),
            method: None,
        })
    }

    /// Matches a single signal within the specified category.
    ///
    /// # Errors
    ///
    /// Returns an error if the category or method is invalid.
    pub fn exact(category: &str, method: &str) -> Result<Self, Error> {
        let mut filter = Self::category(category)?;
        validate_method(method)?;
        filter.method = Some(method.to_owned());
        Ok(filter)
    }

    /// Returns the matching category.
    pub fn category_name(&self) -> &str {
        &self.category
    }

    /// Returns the matching method, if the filter specifies one.
    pub fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }

    pub(crate) fn matches(&self, category: &str, method: &str) -> bool {
        self.category == category && self.method().is_none_or(|name| name == method)
    }
}

/// A received Luna broadcast with its category, method, and payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Signal<T = String> {
    category: String,
    method: String,
    payload: T,
}

impl<T> Signal<T> {
    pub(crate) fn new(category: String, method: String, payload: T) -> Self {
        Self {
            category,
            method,
            payload,
        }
    }

    /// Returns the category on which the signal was sent.
    pub fn category(&self) -> &str {
        &self.category
    }

    /// Returns the signal's method name.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Borrows the payload.
    pub fn payload(&self) -> &T {
        &self.payload
    }

    /// Takes ownership of the payload.
    #[must_use]
    pub fn into_payload(self) -> T {
        self.payload
    }

    fn try_map<U>(self, decode: fn(T) -> Result<U, Error>) -> Result<Signal<U>, Error> {
        Ok(Signal::new(
            self.category,
            self.method,
            decode(self.payload)?,
        ))
    }
}

/// A stream of Luna broadcast signals matching one filter.
#[must_use = "dropping the listener stops receiving signals"]
pub struct SignalStream<T = String> {
    feed: Feed<Signal<String>>,
    cancel: Cancel,
    decode: fn(String) -> Result<T, Error>,
}

impl<T> SignalStream<T> {
    pub(crate) fn new(
        feed: Feed<Signal<String>>,
        cancel: Cancel,
        decode: fn(String) -> Result<T, Error>,
    ) -> Self {
        Self {
            feed,
            cancel,
            decode,
        }
    }

    /// Stops listening and waits for any necessary unregister frame to be written.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection closes or the unregister frame cannot be sent.
    pub async fn cancel(self) -> Result<(), Error> {
        self.cancel.wait().await
    }
}

impl<T> Stream for SignalStream<T> {
    type Item = Result<Signal<T>, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.feed
            .poll_next(cx)
            .map(|item| item.map(|result| result.and_then(|signal| signal.try_map(self.decode))))
    }
}

impl<T> fmt::Debug for SignalStream<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalStream").finish_non_exhaustive()
    }
}

type Terminal = Arc<Mutex<Option<Error>>>;

pub(crate) struct Feed<T> {
    receiver: Pin<Box<Receiver<T>>>,
    terminal: Terminal,
}

pub(crate) struct FeedSender<T> {
    sender: Sender<T>,
    terminal: Terminal,
}

pub(crate) fn feed<T>() -> (FeedSender<T>, Feed<T>) {
    let (sender, receiver) = async_channel::bounded(STREAM_CAPACITY);
    let terminal = Terminal::default();
    (
        FeedSender {
            sender,
            terminal: Arc::clone(&terminal),
        },
        Feed {
            receiver: Box::pin(receiver),
            terminal,
        },
    )
}

impl<T> Feed<T> {
    pub(crate) async fn first(&self) -> Result<T, Error> {
        self.receiver.recv().await.map_err(|_| {
            self.take_error()
                .unwrap_or_else(|| Error::disconnected("Luna closed before sending a reply"))
        })
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<T, Error>>> {
        self.receiver.as_mut().poll_next(cx).map(|item| match item {
            Some(value) => Some(Ok(value)),
            None => self.take_error().map(Err),
        })
    }

    fn take_error(&self) -> Option<Error> {
        self.terminal
            .lock()
            .expect("stream terminal mutex poisoned")
            .take()
    }
}

impl<T> FeedSender<T> {
    pub(crate) fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    pub(crate) fn push(&self, item: T) -> Result<(), Error> {
        self.sender.try_send(item).map_err(|error| match error {
            TrySendError::Full(_) => {
                Error::new(ErrorKind::Overloaded, "Luna stream buffer is full")
            }
            TrySendError::Closed(_) => Error::disconnected("Luna stream was dropped"),
        })
    }

    pub(crate) fn fail(self, error: Error) {
        *self
            .terminal
            .lock()
            .expect("stream terminal mutex poisoned") = Some(error);
        self.sender.close();
    }
}
