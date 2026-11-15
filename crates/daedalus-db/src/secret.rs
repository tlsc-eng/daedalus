/// A password that never prints; callers must opt in with `expose`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_never_shows_the_value() {
        assert_eq!(format!("{:?}", Secret::new("hunter2")), "Secret(***)");
        assert!(!format!("{:?}", Some(Secret::new("x"))).contains('x'));
    }
}
