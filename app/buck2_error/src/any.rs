/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Integrations of `buck2_error::Error` with `anyhow::Error` and `StdError`.

use std::error::request_value;
use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use ref_cast::RefCast;

use crate::error::ErrorKind;
use crate::root::ErrorRoot;

// This implementation is fairly magic and is what allows us to bypass the issue with conflicting
// implementations between `anyhow::Error` and `T: StdError`. The `T: Into<anyhow::Error>` bound is
// what we actually make use of in the implementation, while the other bound is needed to make sure
// this impl does not accidentally cover too many types. Importantly, this impl does not conflict
// with `T: From<T>`
impl<T: fmt::Debug + fmt::Display + Sync + Send + 'static> From<T> for crate::Error
where
    T: Into<anyhow::Error>,
    Result<(), T>: anyhow::Context<(), T>,
{
    #[track_caller]
    #[cold]
    fn from(value: T) -> crate::Error {
        let source_location =
            crate::source_location::from_file(std::panic::Location::caller().file(), None);
        let anyhow: anyhow::Error = value.into();
        recover_crate_error(anyhow.as_ref(), source_location)
    }
}

fn maybe_add_context_from_metadata(mut e: crate::Error, context: &dyn StdError) -> crate::Error {
    if let Some(metadata) = request_value::<ProvidableMetadata>(context) {
        if let Some(category) = metadata.category {
            e = e.context(category);
        }
        if !metadata.tags.is_empty() {
            e = e.tag(metadata.tags.iter().copied());
        }
        e
    } else {
        e
    }
}

pub(crate) fn recover_crate_error(
    value: &'_ (dyn StdError + 'static),
    source_location: Option<String>,
) -> crate::Error {
    // Instead of just turning this into an error root, we'll extract the whole context stack and
    // convert it manually.
    let mut context_stack = Vec::new();
    let mut cur = value;
    // We allow all of these to appear more than once in the context chain, however we always use
    // the bottom-most value when actually generating the root
    let mut source_location = source_location;
    let mut typ = None;
    let mut action_error = None;
    let base = 'base: loop {
        // Handle the `cur` error
        if let Some(base) = cur.downcast_ref::<CrateAsStdError>() {
            break base.0.clone();
        }

        if let Some(metadata) = request_value::<ProvidableMetadata>(cur) {
            source_location = crate::source_location::from_file(
                metadata.source_file,
                metadata.source_location_extra,
            );
            if metadata.typ.is_some() {
                typ = metadata.typ;
            }
            if metadata.action_error.is_some() {
                action_error = metadata.action_error;
            }
        }

        // Compute the next element in the source chain
        if let Some(new_cur) = cur.source() {
            context_stack.push(cur);
            cur = new_cur;
            continue;
        }

        // `anyhow` only ever uses the standard `Display` formatting of error types, never the
        // alternate or debug formatting. We can match that behavior by just converting the error to
        // a string. That prevents us from having to deal with the type returned by `source` being
        // potentially non-`Send` or non-`Sync`.
        let description = format!("{}", cur);
        let e = crate::Error(Arc::new(ErrorKind::Root(Box::new(ErrorRoot::new(
            description,
            typ,
            source_location,
            action_error,
        )))));
        break 'base maybe_add_context_from_metadata(e, cur);
    };
    // We've converted the base error to a `buck2_error::Error`. Next, we need to add back any
    // context that is not included in the `base` error yet.
    let mut e = base;
    for context_value in context_stack.into_iter().rev() {
        // First, just add the value directly. This value is only used for formatting
        e = e.context(format!("{}", context_value));
        // Now add any additional information from the metadata, if it's available
        e = maybe_add_context_from_metadata(e, context_value);
    }
    e
}

impl From<crate::Error> for anyhow::Error {
    #[cold]
    fn from(value: crate::Error) -> Self {
        Into::into(CrateAsStdError(value))
    }
}

#[derive(derive_more::Display, RefCast)]
#[repr(transparent)]
pub(crate) struct CrateAsStdError(pub(crate) crate::Error);

impl fmt::Debug for CrateAsStdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl StdError for CrateAsStdError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match &*self.0.0 {
            ErrorKind::Root(_) => None,
            ErrorKind::WithContext(_, r) | ErrorKind::Emitted(_, r) => {
                Some(CrateAsStdError::ref_cast(r))
            }
        }
    }
}

/// This can be `provide`d by an error to inject buck2-specific information about it.
///
/// For `typ`, `action_error`, and the source information, only the value that appears last in the
/// source chain will be used. The derive macro typically handles this to prevent any surprises,
/// however if this value is being provided manually then care may need to be taken.
#[derive(Clone)]
pub struct ProvidableMetadata {
    pub category: Option<crate::Tier>,
    pub tags: Vec<crate::ErrorTag>,
    pub source_file: &'static str,
    /// Extra information to add to the end of the source location - typically a type/variant name,
    /// and the same thing as gets passed to `buck2_error::source_location::from_file`.
    pub source_location_extra: Option<&'static str>,
    pub typ: Option<crate::ErrorType>,
    /// The protobuf ActionError, if the root was an action error
    pub action_error: Option<buck2_data::ActionError>,
}

#[cfg(test)]
mod tests {
    use std::error::Request;

    use allocative::Allocative;

    use super::*;
    use crate as buck2_error;
    use crate::TypedContext;

    #[derive(Debug, derive_more::Display)]
    struct TestError;

    impl StdError for TestError {}

    fn check_equal(mut a: &crate::Error, mut b: &crate::Error) {
        loop {
            match (&*a.0, &*b.0) {
                (ErrorKind::Root(a), ErrorKind::Root(b)) => {
                    // Avoid comparing vtable pointers
                    assert!(a.test_equal(b));
                    return;
                }
                (
                    ErrorKind::WithContext(a_context, a_inner),
                    ErrorKind::WithContext(b_context, b_inner),
                ) => {
                    a_context.assert_eq(b_context);
                    a = a_inner;
                    b = b_inner;
                }
                (ErrorKind::Emitted(_, a_inner), ErrorKind::Emitted(_, b_inner)) => {
                    a = a_inner;
                    b = b_inner;
                }
                (_, _) => {
                    panic!("Left side did not match right: {:?} {:?}", a, b)
                }
            }
        }
    }

    #[test]
    fn test_roundtrip_no_context() {
        let e = crate::Error::new(TestError).context("context 1");
        let e2 = crate::Error::from(anyhow::Error::from(e.clone()));
        check_equal(&e, &e2);
    }

    #[test]
    fn test_roundtrip_with_context() {
        let e = crate::Error::new(TestError).context("context 1");
        let e2 = crate::Error::from(anyhow::Error::from(e.clone()).context("context 2"));
        let e3 = e.context("context 2");
        check_equal(&e2, &e3);
    }

    #[test]
    fn test_roundtrip_with_typed_context() {
        #[derive(Debug, Allocative, Eq, PartialEq)]
        struct T(usize);
        impl std::fmt::Display for T {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:?}", self)
            }
        }

        impl TypedContext for T {
            fn eq(&self, other: &dyn TypedContext) -> bool {
                match (other as &dyn std::any::Any).downcast_ref::<Self>() {
                    Some(right) => self == right,
                    None => false,
                }
            }
        }

        let e = crate::Error::new(TestError).context(T(1));
        let e2 = crate::Error::from(anyhow::Error::from(e.clone()).context("context 2"));
        let e3 = e.context("context 2");
        check_equal(&e2, &e3);
    }

    #[derive(Debug, derive_more::Display)]
    struct FullMetadataError;

    impl StdError for FullMetadataError {
        fn provide<'a>(&'a self, request: &mut Request<'a>) {
            request.provide_value(ProvidableMetadata {
                typ: Some(crate::ErrorType::Watchman),
                action_error: None,
                source_file: file!(),
                source_location_extra: Some("FullMetadataError"),
                tags: vec![
                    crate::ErrorTag::WatchmanTimeout,
                    crate::ErrorTag::StarlarkFail,
                    crate::ErrorTag::WatchmanTimeout,
                ],
                category: Some(crate::Tier::Tier0),
            });
        }
    }

    #[test]
    fn test_metadata() {
        for e in [
            FullMetadataError.into(),
            crate::Error::new(FullMetadataError),
        ] {
            assert_eq!(e.get_tier(), Some(crate::Tier::Tier0));
            assert_eq!(e.get_error_type(), Some(crate::ErrorType::Watchman));
            assert_eq!(
                e.source_location(),
                Some("buck2_error/src/any.rs::FullMetadataError")
            );
            assert_eq!(
                &e.tags(),
                &[
                    crate::ErrorTag::StarlarkFail,
                    crate::ErrorTag::WatchmanTimeout
                ]
            );
        }
    }

    #[test]
    fn test_metadata_through_anyhow() {
        let e: anyhow::Error = FullMetadataError.into();
        let e = e.context("anyhow");
        let e: crate::Error = e.into();
        assert_eq!(e.get_tier(), Some(crate::Tier::Tier0));
        assert!(format!("{:?}", e).contains("anyhow"));
    }

    #[derive(Debug, thiserror::Error)]
    #[error("wrapper")]
    struct WrapperError(#[source] FullMetadataError);

    #[test]
    fn test_metadata_through_wrapper() {
        let e: crate::Error = WrapperError(FullMetadataError).into();
        assert_eq!(e.get_tier(), Some(crate::Tier::Tier0));
        assert!(format!("{:?}", e).contains("wrapper"));
    }

    #[derive(Debug, buck2_error_derive::Error)]
    #[buck2(tier0)]
    #[error("wrapper2")]
    struct FullMetadataContextWrapperError(#[source] FullMetadataError);

    #[test]
    fn test_context_in_wrapper() {
        let e: crate::Error = FullMetadataContextWrapperError(FullMetadataError).into();
        assert_eq!(e.get_tier(), Some(crate::Tier::Tier0));
        assert_eq!(e.get_error_type(), Some(crate::ErrorType::Watchman));
        assert_eq!(
            e.source_location(),
            Some("buck2_error/src/any.rs::FullMetadataError")
        );
        assert!(format!("{:?}", e).contains("wrapper2"));
    }

    #[derive(Debug, buck2_error_derive::Error)]
    #[buck2(input)]
    #[error("unused")]
    struct UserMetadataError;

    #[derive(Debug, buck2_error_derive::Error)]
    #[buck2(tier0)]
    #[error("unused")]
    struct InfraMetadataWrapperError(#[source] UserMetadataError);

    #[test]
    fn test_no_root_metadata_context() {
        let e = InfraMetadataWrapperError(UserMetadataError);
        let e: crate::Error = e.into();
        assert_eq!(e.get_tier(), Some(crate::Tier::Tier0));
    }
}
