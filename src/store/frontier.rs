#[derive(Clone, Copy, Default)]
pub(super) struct Resources {
    pub concurrency: u64,
    pub storage: u64,
    pub model_spend: u64,
    pub paid_spend: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Competition {
    pub group: String,
    pub uncertainty: String,
    pub rule: String,
}

pub(super) struct Work<T> {
    pub item: T,
    pub write_scopes: Vec<String>,
    pub competition: Option<Competition>,
    pub resources: Resources,
}

pub(super) struct OccupiedWork {
    pub write_scopes: Vec<String>,
    pub competition: Option<Competition>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HoldReason {
    DeclaredWriteOverlap,
    CompleteResourceBudgetUnavailable,
}

impl HoldReason {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::DeclaredWriteOverlap => "declared_write_overlap",
            Self::CompleteResourceBudgetUnavailable => "complete_resource_budget_unavailable",
        }
    }
}

pub(super) struct Frontier<T> {
    pub selected: Vec<T>,
    pub held: Vec<(T, HoldReason)>,
}

pub(super) fn select<T>(
    candidates: Vec<Work<T>>,
    mut occupied: Vec<OccupiedWork>,
    mut used: Resources,
    ceilings: Resources,
) -> Frontier<T> {
    let mut selected = Vec::new();
    let mut held = Vec::new();
    for candidate in candidates {
        let conflicts = occupied.iter().any(|active| {
            scopes_overlap(&candidate.write_scopes, &active.write_scopes)
                && !competitions_match(candidate.competition.as_ref(), active.competition.as_ref())
        });
        if conflicts {
            held.push((candidate.item, HoldReason::DeclaredWriteOverlap));
            continue;
        }
        if !resources_fit(used, candidate.resources, ceilings) {
            held.push((
                candidate.item,
                HoldReason::CompleteResourceBudgetUnavailable,
            ));
            continue;
        }
        reserve(&mut used, candidate.resources);
        occupied.push(OccupiedWork {
            write_scopes: candidate.write_scopes,
            competition: candidate.competition,
        });
        selected.push(candidate.item);
    }
    Frontier { selected, held }
}

pub(super) fn resources_fit(used: Resources, requested: Resources, ceilings: Resources) -> bool {
    used.concurrency
        .checked_add(requested.concurrency)
        .is_some_and(|total| total <= ceilings.concurrency)
        && used
            .storage
            .checked_add(requested.storage)
            .is_some_and(|total| total <= ceilings.storage)
        && used
            .model_spend
            .checked_add(requested.model_spend)
            .is_some_and(|total| total <= ceilings.model_spend)
        && used
            .paid_spend
            .checked_add(requested.paid_spend)
            .is_some_and(|total| total <= ceilings.paid_spend)
}

pub(super) fn scopes_overlap(left: &[String], right: &[String]) -> bool {
    left.iter().any(|left_scope| {
        right.iter().any(|right_scope| {
            path_is_within_scope(left_scope, right_scope)
                || path_is_within_scope(right_scope, left_scope)
        })
    })
}

fn competitions_match(left: Option<&Competition>, right: Option<&Competition>) -> bool {
    left.is_some() && left == right
}

fn reserve(used: &mut Resources, requested: Resources) {
    used.concurrency += requested.concurrency;
    used.storage += requested.storage;
    used.model_spend += requested.model_spend;
    used.paid_spend += requested.paid_spend;
}

fn path_is_within_scope(path: &str, scope: &str) -> bool {
    path == scope || path.starts_with(&format!("{scope}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_scopes_overlap_and_siblings_do_not() {
        assert!(scopes_overlap(&["src".into()], &["src/store.rs".into()]));
        assert!(!scopes_overlap(&["src".into()], &["tests".into()]));
    }

    #[test]
    fn resources_require_every_dimension_to_fit() {
        let used = Resources {
            concurrency: 1,
            storage: 5,
            model_spend: 3,
            paid_spend: 0,
        };
        let ceilings = Resources {
            concurrency: 2,
            storage: 10,
            model_spend: 5,
            paid_spend: 0,
        };
        assert!(resources_fit(
            used,
            Resources {
                concurrency: 1,
                storage: 5,
                model_spend: 2,
                paid_spend: 0,
            },
            ceilings,
        ));
        assert!(!resources_fit(
            used,
            Resources {
                concurrency: 1,
                storage: 5,
                model_spend: 3,
                paid_spend: 0,
            },
            ceilings,
        ));
    }

    #[test]
    fn selection_reserves_in_order_and_explains_holds() {
        let frontier = select(
            vec![
                Work {
                    item: "first",
                    write_scopes: vec!["src".into()],
                    competition: None,
                    resources: Resources {
                        concurrency: 1,
                        storage: 5,
                        model_spend: 1,
                        paid_spend: 0,
                    },
                },
                Work {
                    item: "overlap",
                    write_scopes: vec!["src/lib.rs".into()],
                    competition: None,
                    resources: Resources::default(),
                },
                Work {
                    item: "budget",
                    write_scopes: vec!["tests".into()],
                    competition: None,
                    resources: Resources {
                        concurrency: 2,
                        storage: 0,
                        model_spend: 0,
                        paid_spend: 0,
                    },
                },
            ],
            Vec::new(),
            Resources::default(),
            Resources {
                concurrency: 2,
                storage: 10,
                model_spend: 10,
                paid_spend: 0,
            },
        );
        assert_eq!(frontier.selected, vec!["first"]);
        assert_eq!(
            frontier.held,
            vec![
                ("overlap", HoldReason::DeclaredWriteOverlap),
                ("budget", HoldReason::CompleteResourceBudgetUnavailable),
            ]
        );
    }
}
