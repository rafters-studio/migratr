use thiserror::Error;

/// The error type for migratr operations, generic over the executor's error.
#[derive(Debug, Error)]
pub enum Error<E: std::error::Error + Send + Sync + 'static> {
    /// An error from the executor, carried unchanged.
    #[error(transparent)]
    Executor(E),
}

impl<E: std::error::Error + Send + Sync + 'static> From<E> for Error<E> {
    fn from(source: E) -> Self {
        Self::Executor(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Error, PartialEq)]
    #[error("boom")]
    struct Boom;

    #[test]
    fn executor_error_is_carried_unchanged() {
        let err: Error<Boom> = Boom.into();
        assert_eq!(err.to_string(), "boom");
        let Error::Executor(inner) = err;
        assert_eq!(inner, Boom);
    }
}
