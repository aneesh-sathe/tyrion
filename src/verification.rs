pub struct VerificationOutcome {
    pub outcome: &'static str,
    pub observed: String,
}

pub fn exact_match(expected: &str, observed: &str) -> VerificationOutcome {
    VerificationOutcome {
        outcome: if observed == expected {
            "passed"
        } else {
            "failed"
        },
        observed: observed.to_owned(),
    }
}
