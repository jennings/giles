//! IRC case mapping: which names are the same nick or channel.

/// The server's `CASEMAPPING` (ISUPPORT). Nicks, channels, and accounts are compared, and used in
/// identities and conversation ids, in folded form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseMapping {
    /// `ascii`: only `A`-`Z` fold.
    Ascii,
    /// `rfc1459` (the default when the server doesn't say): also `[]\~` fold to `{}|^`.
    #[default]
    Rfc1459,
    /// `rfc1459-strict`: `[]\` fold to `{}|`, but not `~`.
    Rfc1459Strict,
    /// Anything else (e.g. `precis`, which allows Unicode names): `rfc1459` plus Unicode
    /// lowercase.
    Unicode,
}

impl CaseMapping {
    /// The mapping a `CASEMAPPING` token names.
    pub fn from_token(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "ascii" => Self::Ascii,
            "rfc1459" => Self::Rfc1459,
            "rfc1459-strict" => Self::Rfc1459Strict,
            _ => Self::Unicode,
        }
    }

    /// `name` in folded (lower) case.
    pub fn fold(self, name: &str) -> String {
        let mut folded = String::with_capacity(name.len());
        for c in name.chars() {
            match (self, c) {
                (Self::Ascii, c) => folded.push(c.to_ascii_lowercase()),
                (_, '[') => folded.push('{'),
                (_, ']') => folded.push('}'),
                (_, '\\') => folded.push('|'),
                (Self::Rfc1459 | Self::Unicode, '~') => folded.push('^'),
                (Self::Unicode, c) => folded.extend(c.to_lowercase()),
                (_, c) => folded.push(c.to_ascii_lowercase()),
            }
        }
        folded
    }

    /// Whether `a` and `b` name the same thing.
    pub fn eq(self, a: &str, b: &str) -> bool {
        self.fold(a) == self.fold(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc1459_folds_brackets_and_tilde() {
        let map = CaseMapping::Rfc1459;
        assert_eq!(map.fold("Nick[A]\\~"), "nick{a}|^");
        assert!(map.eq("Giles[m]", "giles{M}"));
        // Only ASCII letters fold.
        assert_eq!(map.fold("Ünï"), "Ünï");
    }

    #[test]
    fn strict_and_ascii_fold_less() {
        assert_eq!(CaseMapping::Rfc1459Strict.fold("A[~]"), "a{~}");
        assert_eq!(CaseMapping::Ascii.fold("A[~]"), "a[~]");
        assert!(!CaseMapping::Ascii.eq("a[", "a{"));
    }

    #[test]
    fn unknown_mappings_also_fold_unicode() {
        assert_eq!(CaseMapping::from_token("precis"), CaseMapping::Unicode);
        assert_eq!(CaseMapping::Unicode.fold("Ünï[~]"), "ünï{^}");
    }

    #[test]
    fn tokens_pick_the_mapping() {
        assert_eq!(CaseMapping::from_token("ascii"), CaseMapping::Ascii);
        assert_eq!(
            CaseMapping::from_token("RFC1459-STRICT"),
            CaseMapping::Rfc1459Strict
        );
        assert_eq!(CaseMapping::from_token("rfc1459"), CaseMapping::Rfc1459);
    }
}
