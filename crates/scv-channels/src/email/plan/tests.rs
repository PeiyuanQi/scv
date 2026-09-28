//! Unit tests for `src/email/plan.rs`.

use super::*;

fn limits() -> Limits {
    Limits {
        settle_seconds: 120,
        max_delay_seconds: 900,
        max_items: 10,
        max_bytes: 12 * 1024,
        max_urgent_per_hour: 4,
        max_messages_per_hour: 6,
        max_messages_per_day: 48,
        max_responses_per_hour: 60,
        quiet: None,
        quiet_urgent: QuietUrgent::Deliver,
        offset: 0,
    }
}

fn item(seq: u64, created_at: u64) -> Queued {
    Queued {
        seq,
        created_at,
        class: Class::Report,
        urgent: false,
        bytes: 500,
    }
}

fn urgent(seq: u64, created_at: u64) -> Queued {
    Queued {
        urgent: true,
        ..item(seq, created_at)
    }
}

fn response(seq: u64, created_at: u64) -> Queued {
    Queued {
        class: Class::Response,
        ..item(seq, created_at)
    }
}

fn sent(at: u64, class: SendClass) -> Logged {
    Logged { at, class }
}

const T: u64 = 1_000_000;

#[test]
fn nothing_queued_is_idle() {
    assert_eq!(plan(T, &[], &[], &limits()), Plan::Idle);
}

#[test]
fn a_digest_waits_for_the_settle_window_then_goes() {
    let queue = [item(1, T), item(2, T + 60)];
    assert_eq!(
        plan(T + 100, &queue, &[], &limits()),
        Plan::Wait {
            until: T + 180,
            paused: false
        }
    );
    assert_eq!(
        plan(T + 180, &queue, &[], &limits()),
        Plan::Send {
            seqs: vec![1, 2],
            class: SendClass::Digest
        }
    );
}

#[test]
fn a_steady_trickle_goes_out_at_the_maximum_delay() {
    let queue: Vec<Queued> = (0..8).map(|n| item(n, T + n * 100)).collect();
    // The last arrived 50 s ago, but the first has waited 750 s.
    assert_eq!(
        plan(T + 750, &queue, &[], &limits()),
        Plan::Wait {
            until: T + 820,
            paused: false
        }
    );
    let mut longer = queue.clone();
    longer.push(item(8, T + 850));
    assert!(matches!(
        plan(T + 900, &longer, &[], &limits()),
        Plan::Send {
            class: SendClass::Digest,
            ..
        }
    ));
}

#[test]
fn a_full_count_goes_at_once_and_a_message_holds_at_most_the_limits() {
    let queue: Vec<Queued> = (0..12).map(|n| item(n, T)).collect();
    let Plan::Send { seqs, class } = plan(T, &queue, &[], &limits()) else {
        panic!("a full queue is due");
    };
    assert_eq!(class, SendClass::Digest);
    assert_eq!(seqs, (0..10).collect::<Vec<_>>());
    let big: Vec<Queued> = (0..10)
        .map(|n| Queued {
            bytes: 5000,
            ..item(n, T)
        })
        .collect();
    let Plan::Send { seqs, .. } = plan(T, &big, &[], &limits()) else {
        panic!("due");
    };
    assert_eq!(seqs, [0, 1]);
    // An item larger than a message still goes, alone.
    let huge = [Queued {
        bytes: 20_000,
        ..item(7, T)
    }];
    assert_eq!(
        plan(T + 200, &huge, &[], &limits()),
        Plan::Send {
            seqs: vec![7],
            class: SendClass::Digest
        }
    );
}

#[test]
fn urgent_items_go_first_and_others_ride_along() {
    let queue = [item(1, T), urgent(2, T + 10), item(3, T + 20)];
    assert_eq!(
        plan(T + 20, &queue, &[], &limits()),
        Plan::Send {
            seqs: vec![2, 1, 3],
            class: SendClass::Urgent
        }
    );
}

#[test]
fn a_spent_urgent_budget_treats_urgent_items_as_normal() {
    let log: Vec<Logged> = (0..4)
        .map(|n| sent(T - 100 - n, SendClass::Urgent))
        .collect();
    let queue = [urgent(1, T)];
    assert_eq!(
        plan(T + 10, &queue, &log, &limits()),
        Plan::Wait {
            until: T + 120,
            paused: false
        }
    );
    assert_eq!(
        plan(T + 120, &queue, &log, &limits()),
        Plan::Send {
            seqs: vec![1],
            class: SendClass::Digest
        }
    );
}

#[test]
fn quiet_hours_across_midnight_hold_digests_and_deliver_or_hold_urgent_mail() {
    // 23:30 UTC+8 is 15:30 UTC.
    let day = 20_000 * 86_400;
    let late = day + 15 * 3600 + 30 * 60;
    let quiet = Limits {
        quiet: Some(QuietHours {
            start: 23 * 60,
            end: 7 * 60 + 30,
        }),
        offset: 8 * 3600,
        ..limits()
    };
    let queue = [item(1, late - 1000), urgent(2, late - 10)];
    // Urgent mail breaks quiet hours with `deliver`, alone.
    assert_eq!(
        plan(late, &queue, &[], &quiet),
        Plan::Send {
            seqs: vec![2],
            class: SendClass::Urgent
        }
    );
    // With `hold`, everything waits for 07:30 local, 23:30 UTC.
    let hold = Limits {
        quiet_urgent: QuietUrgent::Hold,
        ..quiet.clone()
    };
    let end = day + 23 * 3600 + 30 * 60;
    assert_eq!(
        plan(late, &queue, &[], &hold),
        Plan::Wait {
            until: end,
            paused: false
        }
    );
    // Just after midnight local it is still quiet; at the end it is not,
    // and the held items are overdue, so they go at once.
    assert!(matches!(
        plan(day + 16 * 3600 + 60, &queue[..1], &[], &hold),
        Plan::Wait { until, .. } if until == end
    ));
    assert_eq!(
        plan(end, &queue[..1], &[], &hold),
        Plan::Send {
            seqs: vec![1],
            class: SendClass::Digest
        }
    );
    assert!(!quiet_now(end, 8 * 3600, hold.quiet.unwrap()));
    assert!(quiet_now(end - 1, 8 * 3600, hold.quiet.unwrap()));
}

#[test]
fn message_limits_pause_digests_until_the_window_has_room() {
    let log: Vec<Logged> = (0..6)
        .map(|n| sent(T - 3000 + n * 10, SendClass::Digest))
        .collect();
    let queue = [item(1, T - 500)];
    assert_eq!(
        plan(T, &queue, &log, &limits()),
        Plan::Wait {
            until: T - 3000 + 3600,
            paused: true
        }
    );
    let daily: Vec<Logged> = (0..48)
        .map(|n| sent(T - 80_000 + n * 1000, SendClass::Urgent))
        .collect();
    let Plan::Wait { until, paused } = plan(T, &queue, &daily, &limits()) else {
        panic!("the day is full");
    };
    assert!(paused);
    assert_eq!(until, T - 80_000 + 86_400);
}

#[test]
fn responses_go_at_once_under_their_own_limit() {
    let log: Vec<Logged> = (0..6)
        .map(|n| sent(T - 100 - n, SendClass::Digest))
        .collect();
    let queue = [item(1, T - 500), response(2, T)];
    assert_eq!(
        plan(T, &queue, &log, &limits()),
        Plan::Send {
            seqs: vec![2],
            class: SendClass::Response
        }
    );
    let one = Limits {
        max_responses_per_hour: 1,
        ..limits()
    };
    let answered = [sent(T - 100, SendClass::Response)];
    assert_eq!(
        plan(T, &[response(3, T)], &answered, &one),
        Plan::Wait {
            until: T - 100 + 3600,
            paused: false
        }
    );
}

/// A small deterministic generator for the property test.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound
    }
}

#[test]
fn over_random_arrivals_the_limits_hold_and_every_item_goes_out_in_order() {
    for seed in 0..40 {
        let mut random = Lcg(seed);
        let limits = Limits {
            max_urgent_per_hour: random.next(3) as usize,
            max_messages_per_hour: 1 + random.next(4) as usize,
            max_messages_per_day: 10 + random.next(20) as usize,
            max_items: 3 + random.next(8) as usize,
            quiet: (seed % 3 == 0).then_some(QuietHours {
                start: 22 * 60,
                end: 6 * 60,
            }),
            ..limits()
        };
        let mut queue: Vec<Queued> = Vec::new();
        let mut log: Vec<Logged> = Vec::new();
        let mut delivered: Vec<u64> = Vec::new();
        let mut next_seq = 0;
        let start = 20_000 * 86_400;
        let mut now = start;
        // A day of arrivals the limits can carry, then days to drain them.
        while now < start + 4 * 86_400 {
            if now < start + 86_400 && random.next(100) < 2 {
                queue.push(Queued {
                    seq: next_seq,
                    created_at: now,
                    class: Class::Report,
                    urgent: random.next(5) == 0,
                    bytes: 100 + random.next(3000) as usize,
                });
                next_seq += 1;
            }
            // Deterministic: the same inputs give the same plan.
            let decided = plan(now, &queue, &log, &limits);
            assert_eq!(decided, plan(now, &queue, &log, &limits));
            match decided {
                Plan::Send { seqs, class } => {
                    assert!(!seqs.is_empty());
                    assert!(seqs.len() <= limits.max_items);
                    queue.retain(|item| !seqs.contains(&item.seq));
                    delivered.extend(seqs);
                    log.push(Logged { at: now, class });
                }
                Plan::Wait { until, .. } => assert!(until > now, "a wait must end later"),
                Plan::Idle => assert!(queue.is_empty()),
            }
            let window = |span: u64, classes: &[SendClass]| {
                log.iter()
                    .filter(|entry| classes.contains(&entry.class) && now - entry.at < span)
                    .count()
            };
            assert!(window(3600, &[SendClass::Digest]) <= limits.max_messages_per_hour);
            assert!(window(3600, &[SendClass::Urgent]) <= limits.max_urgent_per_hour);
            assert!(
                window(86_400, &[SendClass::Digest, SendClass::Urgent])
                    <= limits.max_messages_per_day
            );
            now += 60;
        }
        assert!(
            queue.is_empty(),
            "seed {seed}: {} items never went",
            queue.len()
        );
        let mut sorted = delivered.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), delivered.len(), "an item went twice");
        assert_eq!(sorted.len() as u64, next_seq);
    }
}
