use std::sync::Arc;

use crate::peer::Subscriber;

#[tokio::test]
async fn create_subscriber_data_channel() {
    let id = "test".to_owned();
    let subscriber = Subscriber::new(id, Arc::default())
        .await.expect("Subscriber::new failed");
    let dc = subscriber.create_data_channel("testchannel".to_owned())
        .await.expect("Subscriber::create_data_channel failed");
    assert!(dc.label().eq("testchannel"), "label should be \"testchannel\"");
}