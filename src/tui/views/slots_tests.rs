//! Tests of the Slots panel's state and screens (`views/slots.rs`).

use crossterm::event::KeyCode;

use super::super::testing::{self, key};
use super::*;
use crate::live::slots::RangeStats;
use crate::tui::app::{App, Effect, Msg, Screen};
use crate::tui::fixtures;

fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

fn ids(app: &App) -> (ConnId, ScanId) {
    let s = app.session.as_ref().unwrap();
    (s.id, s.scan)
}

/// A scanned app whose session is a cluster of `n` nodes, the live connection ready.
fn cluster_app(n: usize) -> App {
    let mut app = testing::scanned();
    let (id, scan) = ids(&app);
    let mut nodes = fixtures::live_nodes();
    if n != 3 {
        nodes = (0..n)
            .map(|i| {
                let mut x = nodes[i % 3].clone();
                x.endpoint.port = 7000 + u16::try_from(i).unwrap();
                x.id = format!("n{i}");
                let a = u16::try_from(16_384 * i / n).unwrap();
                let b = u16::try_from(16_384 * (i + 1) / n - 1).unwrap();
                x.slots = vec![(a, b)];
                x
            })
            .collect();
    }
    app.apply(Msg::Nodes { id, scan, nodes });
    testing::live_ready(&mut app);
    app
}

/// The slots requests among `effects`: (request id, node, ranges).
/// A slots request sent: its id, the node and the ranges.
type Sent = (ReqId, usize, Vec<(u16, u16)>);

fn sent(effects: &[Effect]) -> Vec<Sent> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Live {
                req_id,
                to,
                req: LiveRequest::Slots(Request::Stats { ranges }),
                ..
            } => match to.to {
                crate::live::To::Node(i) => Some((*req_id, i, ranges.clone())),
                crate::live::To::Key(_) => panic!("a slots request must go to a node"),
            },
            _ => None,
        })
        .collect()
}

fn stat(slot: u16, keys: u64, cpu: u64, i: u64, o: u64) -> SlotStat {
    SlotStat {
        slot,
        keys: Some(keys),
        cpu_usec: Some(cpu),
        net_in: Some(i),
        net_out: Some(o),
        memory: None,
    }
}

fn stats(
    flavor: ServerFlavor,
    version: &str,
    ranges: &[(u16, u16)],
    slots: &[SlotStat],
) -> LiveReply {
    LiveReply::Slots(Reply::Stats(NodeStats {
        flavor,
        version: version.into(),
        supported: true,
        ranges: ranges
            .iter()
            .map(|&(start, end)| RangeStats {
                start,
                end,
                slots: slots
                    .iter()
                    .filter(|s| (start..=end).contains(&s.slot))
                    .map(|s| SlotStat {
                        // Memory is a Redis figure.
                        memory: (flavor == ServerFlavor::Redis)
                            .then_some(s.keys.unwrap_or(0) * 100),
                        ..*s
                    })
                    .collect(),
            })
            .collect(),
    }))
}

fn valkey(ranges: &[(u16, u16)], slots: &[SlotStat]) -> LiveReply {
    stats(ServerFlavor::Valkey, "8.1.0", ranges, slots)
}

fn too_old() -> LiveReply {
    LiveReply::Slots(Reply::Stats(NodeStats {
        flavor: ServerFlavor::Redis,
        version: "7.4.2".into(),
        supported: false,
        ranges: Vec::new(),
    }))
}

/// Slots 10 and 11 (node 0), 6000 (node 1) and 12000 (node 2) have figures.
fn figures() -> Vec<SlotStat> {
    vec![
        stat(10, 40, 900, 1_000, 2_000),
        stat(11, 5, 100, 10, 20),
        stat(6000, 90, 300, 5_000, 100),
        stat(12000, 10, 5_000, 50, 90_000),
    ]
}

/// Shows Slots on a 3-node cluster and answers every node.
fn loaded(flavor: ServerFlavor, slots: &[SlotStat]) -> App {
    let mut app = cluster_app(3);
    let out = testing::show(&mut app, Screen::Slots);
    let version = if flavor == ServerFlavor::Valkey {
        "8.1.0"
    } else {
        "8.2.0"
    };
    for (id, _, ranges) in sent(&out) {
        testing::reply(&mut app, id, Ok(stats(flavor, version, &ranges, slots)));
    }
    app
}

#[test]
fn showing_the_panel_asks_each_node_for_its_own_ranges() {
    let mut app = cluster_app(3);
    let out = testing::show(&mut app, Screen::Slots);
    let got = sent(&out);
    let nodes = app.session.as_ref().unwrap().nodes.clone();
    assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), [0, 1, 2]);
    for (_, i, ranges) in &got {
        assert_eq!(*ranges, nodes[*i].slots);
    }
    // Nothing else is sent for this panel. (Getting here passes the Big keys panel,
    // which reads its keys' lengths on their slot owners.)
    let others: Vec<_> = testing::requests(&out)
        .into_iter()
        .filter(|(_, r)| !matches!(r, LiveRequest::Slots(_)))
        .collect();
    assert!(
        others
            .iter()
            .all(|(_, r)| matches!(r, LiveRequest::Keys(_))),
        "{others:?}"
    );
}

#[test]
fn at_most_four_nodes_at_once() {
    let mut app = cluster_app(6);
    let out = testing::show(&mut app, Screen::Slots);
    let first = sent(&out);
    assert_eq!(first.len(), AT_ONCE);
    // Each answer lets the next node go.
    let mut asked: Vec<usize> = first.iter().map(|g| g.1).collect();
    let mut waiting = first;
    while let Some((id, _, ranges)) = waiting.pop() {
        let out = testing::reply(&mut app, id, Ok(valkey(&ranges, &[])));
        let next = sent(&out);
        assert!(next.len() <= 1);
        for n in next {
            asked.push(n.1);
            waiting.push(n);
        }
    }
    asked.sort_unstable();
    assert_eq!(asked, [0, 1, 2, 3, 4, 5]);
}

#[test]
fn it_reads_on_show_and_on_r_and_never_on_a_tick() {
    let mut app = loaded(ServerFlavor::Valkey, &figures());
    let t0 = Instant::now();
    for s in 0..120 {
        let out = app.apply(Msg::TickAt(t0 + std::time::Duration::from_secs(s)));
        assert!(testing::requests(&out).is_empty(), "no poll");
    }
    // Leaving and coming back does not read again.
    app.handle_key(ch('3'));
    let out = testing::show(&mut app, Screen::Slots);
    assert!(testing::requests(&out).is_empty());
    // r does, on every node.
    let out = app.handle_key(ch('r'));
    assert_eq!(sent(&out).len(), 3);
    assert_eq!(testing::requests(&out).len(), 3);
}

#[test]
fn a_connection_that_is_ready_later_is_read_on_the_next_tick_once() {
    let mut app = testing::scanned();
    let (id, scan) = ids(&app);
    app.apply(Msg::Nodes {
        id,
        scan,
        nodes: fixtures::live_nodes(),
    });
    let out = testing::show(&mut app, Screen::Slots);
    assert!(sent(&out).is_empty(), "the connection is not ready");
    testing::live_ready(&mut app);
    let t0 = Instant::now();
    let out = app.apply(Msg::TickAt(t0));
    assert_eq!(sent(&out).len(), 3);
    let out = app.apply(Msg::TickAt(t0 + std::time::Duration::from_secs(1)));
    assert!(sent(&out).is_empty());
}

#[test]
fn a_standalone_server_sends_nothing() {
    let mut app = testing::scanned();
    testing::live_ready(&mut app);
    let out = testing::show(&mut app, Screen::Slots);
    assert!(sent(&out).is_empty());
    let out = app.handle_key(ch('r'));
    assert!(sent(&out).is_empty());
}

#[test]
fn ranges_add_up_their_slots_and_missing_metrics_stay_missing() {
    let app = loaded(ServerFlavor::Valkey, &figures());
    let nodes = app.session.as_ref().unwrap().nodes.clone();
    let rows = app.views.slots.range_rows(&nodes);
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].outcome,
        Outcome::Done(Totals {
            keys: Some(45),
            cpu_usec: Some(1_000),
            net_in: Some(1_010),
            net_out: Some(2_020),
            memory: None,
        })
    );
    assert_eq!(rows[1].start, 5461);
    // A reply without a metric shows none, not 0.
    let t = Totals::of(&[SlotStat {
        slot: 1,
        keys: Some(3),
        ..SlotStat::default()
    }]);
    assert_eq!((t.keys, t.cpu_usec, t.memory), (Some(3), None, None));
    assert_eq!(Totals::of(&[]).keys, None);
    // memory-bytes adds up on Redis.
    let app = loaded(ServerFlavor::Redis, &figures());
    let rows = app.views.slots.range_rows(&nodes);
    assert!(matches!(&rows[0].outcome, Outcome::Done(t) if t.memory == Some(4_500)));
    assert_eq!(app.views.slots.flavor(), Some(ServerFlavor::Redis));
}

#[test]
fn the_hottest_slots_are_ordered_and_o_cycles_the_order() {
    let mut app = loaded(ServerFlavor::Valkey, &figures());
    let slots = |app: &App| {
        app.views
            .slots
            .hottest
            .iter()
            .map(|h| h.stat.slot)
            .collect::<Vec<_>>()
    };
    assert_eq!(app.views.slots.order, Order::Keys);
    assert_eq!(slots(&app), [6000, 10, 12000, 11]);
    // The node of a slot is the node that answered for it.
    assert_eq!(app.views.slots.hottest[0].node, 1);
    app.handle_key(ch('o'));
    assert_eq!(app.views.slots.order, Order::Cpu);
    assert_eq!(slots(&app), [12000, 10, 6000, 11]);
    app.handle_key(ch('o'));
    assert_eq!(app.views.slots.order, Order::NetIn);
    assert_eq!(slots(&app), [6000, 10, 12000, 11]);
    app.handle_key(ch('o'));
    assert_eq!(app.views.slots.order, Order::NetOut);
    assert_eq!(slots(&app), [12000, 10, 6000, 11]);
    app.handle_key(ch('o'));
    assert_eq!(app.views.slots.order, Order::Keys);
    assert_eq!(slots(&app), [6000, 10, 12000, 11]);
}

#[test]
fn only_the_top_twenty_are_kept_and_ties_go_by_slot() {
    let many: Vec<SlotStat> = (0..40u16).rev().map(|s| stat(s, 7, 0, 0, 0)).collect();
    let app = loaded(ServerFlavor::Valkey, &many);
    let h = &app.views.slots.hottest;
    assert_eq!(h.len(), HOTTEST);
    assert_eq!(h[0].stat.slot, 0);
    assert_eq!(h[19].stat.slot, 19);
}

#[test]
fn move_keys_move_through_the_hottest_slots_and_y_copies_nothing() {
    let mut app = loaded(ServerFlavor::Valkey, &figures());
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.views.slots.selected().unwrap().stat.slot, 10);
    app.handle_key(ch('G'));
    assert_eq!(app.views.slots.sel, 3);
    app.handle_key(ch('g'));
    assert_eq!(app.views.slots.sel, 0);
    assert!(app.handle_key(ch('y')).is_empty());
    assert!(app.notice.is_none());
}

#[test]
fn zero_cpu_and_network_is_noticed() {
    let zero = [stat(1, 3, 0, 0, 0), stat(9000, 4, 0, 0, 0)];
    assert!(
        loaded(ServerFlavor::Valkey, &zero)
            .views
            .slots
            .cpu_and_network_zero()
    );
    assert!(
        !loaded(ServerFlavor::Valkey, &figures())
            .views
            .slots
            .cpu_and_network_zero()
    );
    // Nothing read is not a note.
    assert!(!SlotsState::default().cpu_and_network_zero());
}

#[test]
fn an_old_primary_turns_the_whole_panel_off_and_stops_the_queue() {
    let mut app = cluster_app(6);
    let out = testing::show(&mut app, Screen::Slots);
    let first = sent(&out);
    // The first node answers fine, the second is old.
    testing::reply(
        &mut app,
        first[0].0,
        Ok(stats(ServerFlavor::Redis, "8.2.0", &first[0].2, &[])),
    );
    let out = testing::reply(&mut app, first[1].0, Ok(too_old()));
    assert!(sent(&out).is_empty(), "nothing more is sent");
    // Later answers do not bring it back.
    testing::reply(&mut app, first[2].0, Ok(valkey(&first[2].2, &[])));
    assert_eq!(
        app.views.slots.off.as_ref().map(Off::text).as_deref(),
        Some("CLUSTER SLOT-STATS needs Redis 8.2 or Valkey 8.0; 10.0.1.12:7001 runs Redis 7.4.2")
    );
}

#[test]
fn a_refusal_turns_the_panel_off() {
    let mut app = cluster_app(3);
    let out = testing::show(&mut app, Screen::Slots);
    let first = sent(&out);
    let out = testing::reply(
        &mut app,
        first[0].0,
        Err(LiveError::Denied("CLUSTER SLOT-STATS".into())),
    );
    assert!(sent(&out).is_empty());
    assert_eq!(app.views.slots.off, Some(Off::Refused));
    assert_eq!(
        Off::Refused.text(),
        "The server refused CLUSTER SLOT-STATS for this user."
    );
}

#[test]
fn a_failed_node_fails_its_ranges_and_the_others_show() {
    let mut app = cluster_app(3);
    let out = testing::show(&mut app, Screen::Slots);
    let first = sent(&out);
    testing::reply(
        &mut app,
        first[0].0,
        Err(LiveError::Failed("the connection to x was lost".into())),
    );
    testing::reply(&mut app, first[1].0, Ok(valkey(&first[1].2, &figures())));
    let nodes = app.session.as_ref().unwrap().nodes.clone();
    let rows = app.views.slots.range_rows(&nodes);
    assert!(matches!(rows[0].outcome, Outcome::Failed(_)));
    assert!(matches!(rows[1].outcome, Outcome::Done(_)));
    assert_eq!(rows[2].outcome, Outcome::Waiting);
    assert!(app.views.slots.off.is_none());
}

#[test]
fn a_node_discovery_could_not_use_is_not_asked() {
    let mut app = testing::scanned();
    let (id, scan) = ids(&app);
    let mut nodes = fixtures::live_nodes();
    nodes[1].connected = false;
    app.apply(Msg::Nodes { id, scan, nodes });
    testing::live_ready(&mut app);
    let out = testing::show(&mut app, Screen::Slots);
    assert_eq!(sent(&out).iter().map(|g| g.1).collect::<Vec<_>>(), [0, 2]);
    assert!(matches!(app.views.slots.nodes[1], NodeState::Failed(_)));
}

#[test]
fn the_detail_lists_the_scans_keys_of_the_slot_with_their_patterns() {
    let app = testing::scanned();
    let report = app.session.as_ref().unwrap().report.as_ref().unwrap();
    let example = report
        .patterns
        .iter()
        .flat_map(|p| p.examples.iter())
        .next()
        .expect("an example")
        .key
        .clone();
    let slot = key_slot(example.as_bytes());
    let keys = keys_in_slot(report, slot);
    let k = keys.iter().find(|k| k.key == example).expect("the example");
    assert!(k.kinds.contains("example"));
    assert_eq!(k.pattern, pattern_for_key(&example));
    assert!(keys.iter().all(|k| key_slot(k.key.as_bytes()) == slot));
    // A slot no key hashes to has none.
    let used: std::collections::HashSet<u16> = report
        .patterns
        .iter()
        .flat_map(|p| p.examples.iter())
        .map(|k| key_slot(k.key.as_bytes()))
        .collect();
    let empty = (0..16384u16).find(|s| !used.contains(s)).unwrap();
    assert!(keys_in_slot(report, empty).is_empty());
}

#[test]
fn a_rescan_forgets_what_was_read_and_reads_again_when_shown() {
    let mut app = loaded(ServerFlavor::Valkey, &figures());
    assert!(!app.views.slots.hottest.is_empty());
    // A new run of the session.
    app.session.as_mut().unwrap().scan += 1;
    let out = app.apply(Msg::TickAt(Instant::now()));
    assert_eq!(sent(&out).len(), 3);
    assert!(app.views.slots.hottest.is_empty());
}

mod snapshots {
    use super::*;

    fn render(app: &App) -> String {
        format!("{}", testing::render(app, 80, 24).backend())
    }

    #[test]
    fn populated_on_redis_80x24() {
        let app = loaded(ServerFlavor::Redis, &figures());
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn populated_on_valkey_80x24() {
        let mut app = loaded(ServerFlavor::Valkey, &figures());
        app.handle_key(key(KeyCode::Down));
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn zero_cpu_note_80x24() {
        let app = loaded(
            ServerFlavor::Valkey,
            &[stat(10, 40, 0, 0, 0), stat(6000, 9, 0, 0, 0)],
        );
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn failed_node_80x24() {
        let mut app = cluster_app(3);
        let out = testing::show(&mut app, Screen::Slots);
        let first = sent(&out);
        testing::reply(
            &mut app,
            first[0].0,
            Err(LiveError::Failed("the connection to x was lost".into())),
        );
        testing::reply(&mut app, first[1].0, Ok(valkey(&first[1].2, &figures())));
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn off_too_old_80x24() {
        let mut app = cluster_app(3);
        let out = testing::show(&mut app, Screen::Slots);
        testing::reply(&mut app, sent(&out)[1].0, Ok(too_old()));
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn off_refused_80x24() {
        let mut app = cluster_app(3);
        let out = testing::show(&mut app, Screen::Slots);
        testing::reply(
            &mut app,
            sent(&out)[0].0,
            Err(LiveError::Denied("CLUSTER SLOT-STATS".into())),
        );
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn detail_lists_the_keys_of_the_slot_80x24() {
        let mut figures = figures();
        let app = testing::scanned();
        let report = app.session.as_ref().unwrap().report.as_ref().unwrap();
        // Make the busiest slot the one an example key of the scan hashes to.
        let key = report.patterns[0].examples[0].key.clone();
        figures[2].slot = key_slot(key.as_bytes());
        let mut app = loaded(ServerFlavor::Valkey, &figures);
        assert!(
            !keys_in_slot(
                app.session.as_ref().unwrap().report.as_ref().unwrap(),
                figures[2].slot
            )
            .is_empty()
        );
        app.handle_key(ch('g'));
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn off_standalone_80x24() {
        let (app, _) = testing::app_on(Screen::Slots);
        insta::assert_snapshot!(render(&app));
    }
}
