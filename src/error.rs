//! Additional error types designed to retain as much information as possible
//! during a parse failure, making full use of the [`ParseError`] trait.

use std::{
    error::Error,
    fmt::{self, Debug, Display, Formatter, Write},
};

use cascade::cascade;
use indent_write::fmt::IndentWriter;
use joinery::JoinableIterator;
use nom::error::{ContextError, ErrorKind as NomErrorKind, FromExternalError, ParseError};

use crate::final_parser::{ExtractContext, RecreateContext};
use crate::tag::TagError;

/// Enum for generic things that can be expected by nom parsers
///
/// Certain nom parsers (think digit1 or space1) are "low level" in the sense
/// that they scan for a specific character or kind of character. This enum
/// tracks the different kinds of things that can be expected and not found
/// at a location.
///
/// Printing an expectation via `Display` will only include the thing that
/// was expected, in a form suitable for being prefixed with "expected" or
/// suffixed with "was expected".
///
/// This enum is non-exhaustive; it is intended to represent everything parse
/// errors where we know *specifically* what was expected. For instance,
/// take_while cannot create an Expectation, because it can't meaningfully
#[non_exhaustive]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Expectation {
    /// A string tag was expected.
    Tag(&'static str),

    /// A specific character was expected.
    Char(char),

    /// An ASCII letter (`[a-zA-Z]`) was expected.
    Alpha,

    /// A digit (`[0-9]`) was expected.
    Digit,

    /// A hex digit (`[0-9a-fA-F]`) was expected.
    HexDigit,

    /// An oct digit (`[0-7]`) was expected.
    OctDigit,

    /// An alphanumeric character (`[0-9a-zA-Z]`) was expected.
    AlphaNumeric,

    /// A space or tab was expected.
    Space,

    /// A space, tab, newline, or carriage return was expected.
    Multispace,

    /// "\r\n" was expected
    CrLf,

    /// Eof was expected
    // NOTE: currently, ErrorKind::Eof is returned by both eof parsers like
    // eof and all_consuming, as well as non-eof parsers like anychar and
    // nom::number::*. Expectation::Eof is therefore unused for now.
    Eof,
}

impl Display for Expectation {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            Expectation::Tag(tag) => write!(f, "{:?}", tag),
            Expectation::Char(c) => write!(f, "{:?}", c),
            Expectation::Alpha => write!(f, "an ascii letter"),
            Expectation::Digit => write!(f, "an ascii digit"),
            Expectation::HexDigit => write!(f, "a hexadecimal digit"),
            Expectation::OctDigit => write!(f, "an octal digit"),
            Expectation::AlphaNumeric => write!(f, "an ascii alphanumeric character"),
            Expectation::Space => write!(f, "a space or tab"),
            Expectation::Multispace => write!(f, "whitespace"),
            Expectation::Eof => write!(f, "eof"),
            Expectation::CrLf => write!(f, "CRLF"),
        }
    }
}

// TODO: Split BaseErrorKind into two separate types: one for actual base
// errors, which live at the leaves of the tree, and one for
/// These are the different specific things that can go wrong at a particular
/// location during a nom parse. Many of these are collected into an
/// [`ErrorTree`]. See also [`ContextErrorKind`], which is similar, but
/// represents different kinds of contexts that can be attached to a
///  [`BaseErrorKind`] in an [`ErrorTree::Stack`].
#[derive(Debug)]
pub enum BaseErrorKind {
    /// Something specific was expected.
    Expected(Expectation),

    /// A nom parser failed.
    Kind(NomErrorKind),

    /// A [`context`][nom::error::context] combinator attached additional
    /// context at a location to an error from a subparser.
    Context(&'static str),

    /// An error outside of nom occurred during parsing; for instance, as a
    /// result of an error during `map_res`.
    // Design note: I've gone back and forth on whether or not to exclude the
    // ErrorKind from this variant. Right now I'm doing so, because it seems
    // like in practice it's *always* MapRes.
    External(Box<dyn Error + Send + Sync + 'static>),
}

impl Display for BaseErrorKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            BaseErrorKind::Expected(expectation) => write!(f, "expected {}", expectation),
            BaseErrorKind::External(ref err) => write!(f, "external error: \"{}\"", err),
            BaseErrorKind::Kind(kind) => write!(f, "while parsing {:?}", kind),
            BaseErrorKind::Context(context) => write!(f, "in section '{}'", context),
        }
    }
}

/// A comprehensive tree of nom errors describing a parse failure.
///
/// This Error type is designed to be [`VerboseError`]`++`. While
/// [`VerboseError`] can represent a *stack* of errors, this type can represent
/// a full tree. In addition to representing a particular specific parse error,
/// it can also represent a stack of nested error contexts (for instance, as
/// provided by [`context`][nom::error::context]), or a list of alternatives
/// that were all tried individually and all failed.
///
/// In general, the design goal for this type is to discard as little useful
/// information as possible. That being said, many [`nom::ErrorKind`] variants
/// add very little useful contextual information to error traces; for example,
/// [`ErrorKind::Alt`] doesn't add any interesting context to an
/// [`ErrorTree::Alt`], and its presence in a stack precludes merging together
/// adjacent sets of [`ErrorTree::Alt`] siblings
///
/// [`VerboseError`]: nom::error::VerboseError
#[derive(Debug)]
pub enum ErrorTree<I> {
    /// A specific error event at a specific location. Often this will indicate
    /// that something like a tag or character was expected at that location.
    /// When used as part of a stack, it indicates some additional context for
    /// the root error of the stack.
    Base {
        /// The specific thing that went wrong; see [`BaseErrorKind`] for
        /// details.
        kind: BaseErrorKind,

        /// Position of the error in the input data.
        location: I,
    },

    /// A stack indicates a chain of error contexts was provided. The stack
    /// should be read "backwards"; that is, errors *earlier* in the `Vec`
    /// occurred "sooner" (deeper in the call stack).
    Stack(Vec<Self>),

    /// A series of parsers were tried in order at the same location (for
    /// instance, via the [`alt`](nom::branch::alt) combinator) and all of
    /// them failed. All of the errors in this set are "siblings".
    Alt(Vec<Self>),
    // TODO: in a future version of nom-supreme, elaborate on the specific
    // type combinations here. For instance:
    // - Alt can only contain Stack or Base
    // - Stack has a single Base or Alt, followed by a series of contexts
    //   (Context or Kind)
}

impl<I> ErrorTree<I> {
    /// Helper for `map_locations`. Because it operates recursively, this
    /// method uses an `&mut impl FnMut`, which can be reborrowed.
    fn map_locations_ref<T>(self, convert_location: &mut impl FnMut(I) -> T) -> ErrorTree<T> {
        // TODO: does the recursive nature of this function present a potential
        // security risk? Consider replacing it with a breadth-first algorithm,
        // or capping the maximum recursion depth. Note, though, that recursion
        // only happens when alternating between different *kinds* of
        // ErrorTree; nested groups of Alt or Stack are flattened.
        match self {
            ErrorTree::Base { location, kind } => ErrorTree::Base {
                location: convert_location(location),
                kind,
            },
            ErrorTree::Stack(stack) => ErrorTree::Stack(
                stack
                    .into_iter()
                    .map(|err| err.map_locations_ref(convert_location))
                    .collect(),
            ),
            ErrorTree::Alt(siblings) => ErrorTree::Alt(
                siblings
                    .into_iter()
                    .map(|err| err.map_locations_ref(convert_location))
                    .collect(),
            ),
        }
    }

    /// Convert all of the locations in this error using some kind of mapping
    /// function. This is intended to help add additional context that may not
    /// have been available when the nom parsers were running, such as line
    /// and column numbers.
    pub fn map_locations<T>(self, mut convert_location: impl FnMut(I) -> T) -> ErrorTree<T> {
        self.map_locations_ref(&mut convert_location)
    }
}

impl<I: Display> Display for ErrorTree<I> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ErrorTree::Base { kind, location } => write!(f, "{} at {:#}", kind, location),
            ErrorTree::Stack(stack) => {
                write!(f, "{}", stack.iter().rev().join_with(",\n"))
            }
            ErrorTree::Alt(siblings) => {
                writeln!(f, "one of:")?;
                let mut f = IndentWriter::new("  ", f);
                write!(f, "{}", siblings.iter().join_with(", or\n"))
            }
        }
    }
}

impl<I: Display + Debug> Error for ErrorTree<I> {}

impl<I> ParseError<I> for ErrorTree<I> {
    /// Create a new error at the given position
    fn from_error_kind(location: I, kind: NomErrorKind) -> Self {
        let kind = match kind {
            NomErrorKind::Alpha => BaseErrorKind::Expected(Expectation::Alpha),
            NomErrorKind::Digit => BaseErrorKind::Expected(Expectation::Digit),
            NomErrorKind::HexDigit => BaseErrorKind::Expected(Expectation::HexDigit),
            NomErrorKind::OctDigit => BaseErrorKind::Expected(Expectation::OctDigit),
            NomErrorKind::AlphaNumeric => BaseErrorKind::Expected(Expectation::AlphaNumeric),
            NomErrorKind::Space => BaseErrorKind::Expected(Expectation::Space),
            NomErrorKind::MultiSpace => BaseErrorKind::Expected(Expectation::Multispace),
            NomErrorKind::CrLf => BaseErrorKind::Expected(Expectation::CrLf),
            kind => BaseErrorKind::Kind(kind),
        };

        ErrorTree::Base { location, kind }
    }

    /// Combine an existing error with a new one. This is how
    /// error context is accumulated when backtracing. "other" is the original
    /// error, and the inputs new error from higher in the call stack.
    fn append(input: I, kind: NomErrorKind, other: Self) -> Self {
        let stack = cascade! {
            match other {
                // Don't create a stack of [ErrorKind::Alt, ErrorTree::Alt]
                alt @ ErrorTree::Alt(..) if kind == NomErrorKind::Alt => return alt,
                ErrorTree::Stack(stack) => stack,
                err => cascade! {
                    Vec::with_capacity(2);
                    ..push(err);
                }
            };
            ..push(Self::from_error_kind(input, kind));
        };

        ErrorTree::Stack(stack)
    }

    /// Create an error indicating an expected character at a given position
    fn from_char(location: I, character: char) -> Self {
        ErrorTree::Base {
            location,
            kind: BaseErrorKind::Expected(Expectation::Char(character)),
        }
    }

    /// Combine two errors from branches of alt
    fn or(self, other: Self) -> Self {
        let siblings = match (self, other) {
            (ErrorTree::Alt(mut siblings1), ErrorTree::Alt(mut siblings2)) => {
                if siblings1.capacity() >= siblings2.capacity() {
                    siblings1.extend(siblings2);
                    siblings1
                } else {
                    siblings2.extend(siblings1);
                    siblings2
                }
            }
            (ErrorTree::Alt(mut siblings), err) | (err, ErrorTree::Alt(mut siblings)) => {
                siblings.push(err);
                siblings
            }
            (err1, err2) => vec![err1, err2],
        };

        ErrorTree::Alt(siblings)
    }
}

impl<I> ContextError<I> for ErrorTree<I> {
    /// Similar to append: Create a new error with some added context
    fn add_context(location: I, ctx: &'static str, other: Self) -> Self {
        let stack = cascade! {
            match other {
                ErrorTree::Stack(stack) => stack,
                err => cascade! {
                    Vec::with_capacity(2);
                    ..push(err);
                }
            };
            ..push(ErrorTree::Base {
                location,
                kind: BaseErrorKind::Context(ctx),
            });
        };

        ErrorTree::Stack(stack)
    }
}

impl<I, E: Error + Send + Sync + 'static> FromExternalError<I, E> for ErrorTree<I> {
    /// Create an error from a given external error, such as from FromStr
    fn from_external_error(location: I, _kind: NomErrorKind, e: E) -> Self {
        ErrorTree::Base {
            location,
            kind: BaseErrorKind::External(Box::new(e)),
        }
    }
}

impl<I> TagError<I, &'static str> for ErrorTree<I> {
    fn from_tag(location: I, tag: &'static str) -> Self {
        ErrorTree::Base {
            location,
            kind: BaseErrorKind::Expected(Expectation::Tag(tag)),
        }
    }
}

impl<I, T> ExtractContext<I, ErrorTree<T>> for ErrorTree<I>
where
    I: Clone,
    T: RecreateContext<I>,
{
    fn extract_context(self, original_input: I) -> ErrorTree<T> {
        self.map_locations(move |location| T::recreate_context(original_input.clone(), location))
    }
}
