//! Checks that the container helpers work against both servers.
#![cfg(feature = "integration-tests")]

mod common;

async fn ping(server: &common::TestServer) {
    let mut conn = server.admin().await;
    let pong: String = redis::cmd("PING").query_async(&mut conn).await.unwrap();
    assert_eq!(pong, "PONG");
}

#[tokio::test]
async fn redis_container_answers_ping() {
    ping(&common::redis().await).await;
}

#[tokio::test]
async fn valkey_container_answers_ping() {
    ping(&common::valkey().await).await;
}

#[tokio::test]
async fn acl_user_can_be_created_from_args() {
    let server = common::redis_with(&[
        "--user",
        "lens",
        "on",
        ">lens-pw",
        "~*",
        "-@all",
        "+ping",
        "+hello",
        "+client|setinfo",
        "+client|setname",
    ])
    .await;
    let url = format!("redis://lens:lens-pw@127.0.0.1:{}", server.port);
    let mut conn = redis::Client::open(url)
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let pong: String = redis::cmd("PING").query_async(&mut conn).await.unwrap();
    assert_eq!(pong, "PONG");
}
