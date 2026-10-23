//! Typed identifiers and validated names.
//!
//! None of these values carries authority. A [`HandleId`] is an index into the
//! table the kernel keeps for the authenticated sender, so a fabricated or
//! borrowed number can only ever name something the sender already holds.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorCode};

macro_rules! numeric_id {
    ($(#[$doc:meta])* $name:ident, $inner:ty) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub $inner);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

numeric_id!(
    /// A runtime principal. Assigned by the kernel; never accepted from a payload
    /// as proof of who is speaking.
    ProcessId,
    u64
);
numeric_id!(
    /// Index into the sender's own handle table.
    HandleId,
    u32
);
numeric_id!(
    /// Chosen by the caller; unique among that caller's outstanding requests.
    RequestId,
    u64
);
numeric_id!(
    /// Chosen by the kernel to identify one delivery of a call to a provider.
    DispatchId,
    u64
);
numeric_id!(
    /// Incremented each time an endpoint name is bound to a new provider.
    Generation,
    u64
);

/// Longest accepted name, action, or resource segment, in bytes.
pub const MAX_NAME_BYTES: usize = 64;
/// Deepest accepted resource path.
pub const MAX_RESOURCE_DEPTH: usize = 32;

fn validate_name(kind: &str, value: &str) -> Result<(), Error> {
    let ok = !value.is_empty()
        && value.len() <= MAX_NAME_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.'))
        && value != "."
        && value != "..";
    if ok {
        Ok(())
    } else {
        Err(Error::new(
            ErrorCode::InvalidInput,
            format!("invalid {kind}: expected 1-{MAX_NAME_BYTES} bytes of [a-z0-9_.-]"),
        ))
    }
}

macro_rules! validated_name {
    ($(#[$doc:meta])* $name:ident, $kind:literal) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps `value`.
            pub fn new(value: impl Into<String>) -> Result<Self, Error> {
                let value = value.into();
                validate_name($kind, &value)?;
                Ok(Self(value))
            }

            /// The validated text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;
            fn try_from(value: String) -> Result<Self, Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

validated_name!(
    /// Name under which a service interface is provided. Knowing a name grants
    /// nothing; calls require a handle.
    EndpointName,
    "endpoint name"
);
validated_name!(
    /// An action defined by a service interface.
    ActionName,
    "action name"
);
validated_name!(
    /// A launcher-configured program that may be spawned.
    ProgramName,
    "program name"
);

/// Hierarchical resource scope. The kernel compares paths segment by segment
/// and never interprets them; the owning service maps them onto real resources
/// and must re-validate at actual access.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct ResourcePath(Vec<String>);

impl ResourcePath {
    /// The scope containing every resource of an endpoint.
    pub fn root() -> Self {
        Self(Vec::new())
    }

    /// Validates each segment. Segments use a wider alphabet than names but may
    /// not be empty, `.`/`..`, or contain `/`, `\`, or control characters.
    pub fn new<I, S>(segments: I) -> Result<Self, Error>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let segments: Vec<String> = segments.into_iter().map(Into::into).collect();
        if segments.len() > MAX_RESOURCE_DEPTH {
            return Err(Error::new(ErrorCode::InvalidInput, "resource path too deep"));
        }
        for segment in &segments {
            let ok = !segment.is_empty()
                && segment.len() <= MAX_NAME_BYTES
                && segment != "."
                && segment != ".."
                && !segment.chars().any(|c| c == '/' || c == '\\' || c.is_control());
            if !ok {
                return Err(Error::new(ErrorCode::InvalidInput, "invalid resource path segment"));
            }
        }
        Ok(Self(segments))
    }

    /// The validated segments.
    pub fn segments(&self) -> &[String] {
        &self.0
    }

    /// True when `self` is `scope` or lies beneath it.
    pub fn is_within(&self, scope: &ResourcePath) -> bool {
        self.0.len() >= scope.0.len() && self.0.iter().zip(&scope.0).all(|(a, b)| a == b)
    }
}

impl TryFrom<Vec<String>> for ResourcePath {
    type Error = Error;
    fn try_from(value: Vec<String>) -> Result<Self, Error> {
        Self::new(value)
    }
}

impl From<ResourcePath> for Vec<String> {
    fn from(value: ResourcePath) -> Vec<String> {
        value.0
    }
}

impl fmt::Display for ResourcePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/{}", self.0.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_reject_traversal_and_uppercase() {
        assert!(EndpointName::new("fs").is_ok());
        assert!(EndpointName::new("..").is_err());
        assert!(EndpointName::new("Fs").is_err());
        assert!(EndpointName::new("").is_err());
        assert!(EndpointName::new("a".repeat(MAX_NAME_BYTES + 1)).is_err());
    }

    #[test]
    fn resource_paths_reject_escape_segments() {
        for bad in ["..", ".", "", "a/b", "a\\b", "a\0b"] {
            assert!(ResourcePath::new(["ok", bad]).is_err(), "{bad:?}");
        }
        assert!(ResourcePath::new(vec!["x"; MAX_RESOURCE_DEPTH + 1]).is_err());
    }

    #[test]
    fn within_is_a_segment_prefix_not_a_string_prefix() -> Result<(), Error> {
        let scope = ResourcePath::new(["work", "a"])?;
        assert!(ResourcePath::new(["work", "a"])?.is_within(&scope));
        assert!(ResourcePath::new(["work", "a", "f.txt"])?.is_within(&scope));
        assert!(!ResourcePath::new(["work", "ab"])?.is_within(&scope));
        assert!(!ResourcePath::new(["work"])?.is_within(&scope));
        assert!(scope.is_within(&ResourcePath::root()));
        Ok(())
    }

    #[test]
    fn deserialization_validates() {
        assert!(serde_json::from_str::<ResourcePath>(r#"["a",".."]"#).is_err());
        assert!(serde_json::from_str::<ActionName>(r#""READ""#).is_err());
        assert!(serde_json::from_str::<ActionName>(r#""read""#).is_ok());
    }
}
