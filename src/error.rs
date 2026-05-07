use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error(
        "skip threshold exceeded: {skipped}/{total} ({pct:.1}%) candidates skipped, limit {limit:.1}%"
    )]
    SkipThresholdExceeded {
        skipped: usize,
        total: usize,
        pct: f64,
        limit: f64,
    },

    #[error("NetBox error: {0}")]
    Netbox(#[from] anyhow::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Map an error to its documented process exit code (§6.2).
///
/// | Code | Meaning                                     |
/// | ---- | ------------------------------------------- |
/// | 1    | Generic / unexpected error                  |
/// | 2    | Skip threshold exceeded                     |
/// | 3    | Configuration error                         |
#[must_use]
pub fn exit_code_for(err: &anyhow::Error) -> i32 {
    if let Some(inner) = err.downcast_ref::<Error>() {
        match inner {
            Error::Config(_) => 3,
            Error::SkipThresholdExceeded { .. } => 2,
            _ => 1,
        }
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, exit_code_for};

    #[test]
    fn config_error_maps_to_3() {
        let e: anyhow::Error = Error::Config("bad".into()).into();
        assert_eq!(exit_code_for(&e), 3);
    }

    #[test]
    fn skip_threshold_maps_to_2() {
        let e: anyhow::Error = Error::SkipThresholdExceeded {
            skipped: 10,
            total: 20,
            pct: 50.0,
            limit: 5.0,
        }
        .into();
        assert_eq!(exit_code_for(&e), 2);
    }

    #[test]
    fn io_error_maps_to_1() {
        let e: anyhow::Error = Error::Io(std::io::Error::other("boom")).into();
        assert_eq!(exit_code_for(&e), 1);
    }

    #[test]
    fn arbitrary_error_maps_to_1() {
        let e = anyhow::anyhow!("something else");
        assert_eq!(exit_code_for(&e), 1);
    }
}
