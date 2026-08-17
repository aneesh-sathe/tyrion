use crate::domain::EvidenceOutcome;

pub struct VerificationOutcome {
    pub outcome: EvidenceOutcome,
    pub observed: String,
}

pub fn exact_match(expected: &str, observed: &str) -> VerificationOutcome {
    VerificationOutcome {
        outcome: if observed == expected {
            EvidenceOutcome::Passed
        } else {
            EvidenceOutcome::Failed
        },
        observed: observed.to_owned(),
    }
}
