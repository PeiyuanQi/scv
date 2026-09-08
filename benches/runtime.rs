use criterion::{Criterion, black_box, criterion_group, criterion_main};
use peon_core::{BudgetContextPolicy, ContextConfig, ContextPolicy, Message};
use peon_protocol::{ClientMessage, ServerEvent};

fn protocol(c: &mut Criterion) {
    let event = ServerEvent::AssistantDelta {
        request_id: "request".into(),
        session_id: "session".into(),
        turn_id: "turn".into(),
        seq: 42,
        content: "A short streamed response fragment.".repeat(8),
    };
    let encoded = serde_json::to_vec(&event).unwrap();
    c.bench_function("protocol_encode", |b| {
        b.iter(|| serde_json::to_vec(black_box(&event)).unwrap())
    });
    c.bench_function("protocol_decode", |b| {
        b.iter(|| serde_json::from_slice::<ServerEvent>(black_box(&encoded)).unwrap())
    });

    let request = ClientMessage::TurnStart {
        request_id: "request".into(),
        session_id: "session".into(),
        prompt: "hello".into(),
    };
    c.bench_function("client_round_trip", |b| {
        b.iter(|| {
            let bytes = serde_json::to_vec(black_box(&request)).unwrap();
            serde_json::from_slice::<ClientMessage>(&bytes).unwrap()
        })
    });
}

fn context(c: &mut Criterion) {
    let history: Vec<Message> = (0..5_000)
        .flat_map(|index| {
            [
                Message::User {
                    content: format!("user message {index}"),
                },
                Message::Assistant {
                    content: format!("assistant response {index}"),
                    tool_calls: Vec::new(),
                },
            ]
        })
        .collect();
    let policy = BudgetContextPolicy::new(ContextConfig {
        max_tokens: 40_000,
        reserve_output_tokens: 2_000,
        safety_margin_tokens: 1_000,
        bytes_per_token: 3,
        summary_max_chars: 2_000,
    })
    .unwrap();
    c.bench_function("context_select_10000_messages", |b| {
        b.iter(|| policy.select(black_box(&history), "system", &[]).unwrap())
    });
}

criterion_group!(benches, protocol, context);
criterion_main!(benches);
