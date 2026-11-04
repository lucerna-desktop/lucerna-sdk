//! O protocolo do `lucerna-hello`, em processo, num socket temporário.

mod common;

use std::time::Duration;

use common::start;
use lucerna_ipc::raw::RawConn;
use serde_json::{Value, json};

#[tokio::test]
async fn handshake() {
    let r = start(None).await;
    let mut c = RawConn::connect(&r.path).await.unwrap();
    let v = c.hello("hello").await.unwrap();
    let res = &v["result"];
    assert_eq!(res["app"], "hello");
    assert_eq!(res["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(res["topics"], json!(["counter", "reminder"]));
    assert_eq!(res["streams"], json!(["ticks"]));
    let methods: Vec<&str> = res["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    for m in [
        "counter.add",
        "counter.reset",
        "quiet.set",
        "reminder.set",
        "reminder.cancel",
    ] {
        assert!(methods.contains(&m), "falta {m} em {methods:?}");
    }
}

#[tokio::test]
async fn incompatible_version() {
    let r = start(None).await;
    let mut c = RawConn::connect(&r.path).await.unwrap();
    c.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "lucerna.hello",
        "params": {"protocol": "2.0", "client": "test", "app": "hello"},
    }))
    .await
    .unwrap();
    let v = c.recv().await.unwrap().unwrap();
    assert_eq!(v["id"], 1);
    assert_eq!(v["error"]["code"], -32010);
    assert!(c.recv().await.unwrap().is_none(), "a conexão devia fechar");
}

async fn call(c: &mut RawConn, id: i64, method: &str, params: Value) -> Value {
    c.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
        .await
        .unwrap();
    let v = c.recv().await.unwrap().unwrap();
    assert_eq!(v["id"], id);
    v
}

#[tokio::test]
async fn request() {
    let r = start(None).await;
    let mut c = r.raw().await;
    let v = call(&mut c, 1, "counter.add", json!({"n": 3})).await;
    assert_eq!(v["result"], json!({"value": 3}));
    let v = call(&mut c, 2, "counter.add", json!({"n": "3"})).await;
    assert_eq!(v["error"]["code"], -32602);
    let v = call(&mut c, 3, "counter.reset", json!({})).await;
    assert_eq!(v["result"], json!({"value": 0}));
    let v = call(&mut c, 4, "counter.add", json!({"n": i64::MAX})).await;
    assert_eq!(v["result"], json!({"value": i64::MAX}));
    let v = call(&mut c, 5, "counter.add", json!({"n": i64::MAX})).await;
    assert_eq!(
        v["error"],
        json!({"code": 1, "message": "O contador passou do limite."})
    );
    // O estouro não mexe no valor.
    let v = call(&mut c, 6, "counter.add", json!({"n": 0})).await;
    assert_eq!(v["result"], json!({"value": i64::MAX}));
}

#[tokio::test]
async fn subscription_snapshot_and_changes() {
    let r = start(None).await;
    let c = r.client().await;
    let snap = c.subscribe(&["counter"]).await.unwrap();
    assert_eq!(snap.len(), 1);
    assert_eq!((snap[0].seq, &snap[0].data), (0, &json!({"value": 0})));
    c.call("counter.add", Some(json!({"n": 2}))).await.unwrap();
    let s = c.next_state().await.unwrap();
    assert_eq!((s.topic.as_str(), s.seq), ("counter", 1));
    assert_eq!(s.data, json!({"value": 2}));
    c.call("counter.reset", None).await.unwrap();
    let s = c.next_state().await.unwrap();
    assert_eq!((s.seq, s.data), (2, json!({"value": 0})));
}

#[tokio::test]
async fn two_clients() {
    let r = start(None).await;
    let a = r.client().await;
    let b = r.client().await;
    a.subscribe(&["counter"]).await.unwrap();
    b.call("counter.add", Some(json!({"n": 1}))).await.unwrap();
    let s = a.next_state().await.unwrap();
    assert_eq!(s.data, json!({"value": 1}));
    let snap = b.subscribe(&["counter"]).await.unwrap();
    assert_eq!(snap[0].data, json!({"value": 1}));
}

#[tokio::test]
async fn slow_client_on_a_stream() {
    let r = start(None).await;
    let mut c = r.raw().await;
    c.send(
        &json!({"jsonrpc":"2.0","id":1,"method":"lucerna.subscribe","params":{"topics":["ticks"]}}),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await; // não lê: ~2000 quadros produzidos
    // Lê o que ficou no caminho: depois do que coube no socket vem só o quadro
    // mais novo (acima de 1000), não a fila inteira.
    let mut ticks = vec![];
    while ticks.last().is_none_or(|&t| t <= 1000) {
        let v = c.recv().await.unwrap().expect("conexão aberta"); // recv desiste em 5 s
        if v["id"] == 1 {
            continue; // a resposta do subscribe (o retrato, ainda vazio)
        }
        assert_eq!(v["method"], "lucerna.state", "{v}");
        assert_eq!(v["params"]["data"]["bands"].as_array().unwrap().len(), 64);
        ticks.push(v["params"]["data"]["tick"].as_u64().unwrap());
        assert!(ticks.len() < 5000, "fila sem limite");
    }
    assert!(
        ticks.windows(2).any(|w| w[1] - w[0] > 1),
        "houve descarte de quadros intermediários"
    );
    // Para a produção e esvazia: o laço termina na resposta do unsubscribe.
    c.send(&json!({"jsonrpc":"2.0","id":2,"method":"lucerna.unsubscribe","params":{"topics":["ticks"]}}))
        .await
        .unwrap();
    loop {
        let v = c.recv().await.unwrap().expect("conexão aberta");
        if v["id"] == 2 {
            assert_eq!(v["result"], json!({}));
            break;
        }
        if v["method"] == "lucerna.state" {
            ticks.push(v["params"]["data"]["tick"].as_u64().unwrap());
        }
        assert!(ticks.len() < 5000, "fila sem limite");
    }
    assert!(ticks.windows(2).all(|w| w[0] < w[1]), "{ticks:?}");
    // O produtor para sem assinante: o `seq` não anda mais.
    tokio::time::timeout(Duration::from_secs(1), r.ticks().wait_unsubscribed())
        .await
        .unwrap();
    let seq = r.ticks().current().seq;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        r.ticks().current().seq,
        seq,
        "o produtor continuou sem assinante"
    );
}
