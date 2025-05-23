use crate::blueprint::Blueprint;
use crate::blueprint::conversions::raw_identifiers2callable;
use crate::blueprint::middleware::RegisteredWrappingMiddleware;
use crate::blueprint::reflection::{RawIdentifiers, WithLocation};
use pavex_bp_schema::Callable;

/// A middleware that has been configured but has not yet been registered with a [`Blueprint`].
///
/// # Guide
///
/// Check out [`Blueprint::wrap`] for an introduction to wrapping middlewares in Pavex.
#[derive(Clone, Debug)]
pub struct WrappingMiddleware {
    pub(in crate::blueprint) callable: Callable,
    pub(in crate::blueprint) error_handler: Option<Callable>,
}

impl crate::blueprint::middleware::WrappingMiddleware {
    /// Create a new (unregistered) wrapping middleware.
    ///
    /// Check out the documentation of [`Blueprint::wrap`] for more details
    /// on middleware.
    #[track_caller]
    pub fn new(callable: WithLocation<RawIdentifiers>) -> Self {
        Self {
            callable: raw_identifiers2callable(callable),
            error_handler: None,
        }
    }

    /// Register an error handler for this middleware.
    ///
    /// Check out the documentation of [`RegisteredWrappingMiddleware::error_handler`] for more details.
    #[track_caller]
    pub fn error_handler(mut self, error_handler: WithLocation<RawIdentifiers>) -> Self {
        self.error_handler = Some(raw_identifiers2callable(error_handler));
        self
    }

    /// Register this middleware with a [`Blueprint`].
    ///
    /// Check out the documentation of [`Blueprint::wrap`] for more details.
    pub fn register(self, bp: &mut Blueprint) -> RegisteredWrappingMiddleware {
        bp.register_wrapping_middleware(self)
    }
}
