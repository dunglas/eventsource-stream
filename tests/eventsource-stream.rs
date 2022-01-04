use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use http::response::Builder;
use reqwest::ResponseBuilderExt;
use reqwest::Response;
use url::Url;

#[tokio::test]
async fn populate_fields() {
    let url = Url::parse("https://example.com").unwrap();
    let response = Builder::new()
        .status(200)
        .url(url.clone())
        .body("event: my-event\r\ndata:line1
data: line2
:
id: my-id
:should be ignored too\rretry:42
")
        .unwrap();
    let response = Response::from(response);
    let mut stream = response.bytes_stream().eventsource();

    let event = stream.next().await.unwrap().unwrap();
    assert_eq!("my-event", event.event.unwrap());
    assert_eq!("line1
line2", String::from_utf8_lossy(&event.data));
    assert_eq!("my-id", String::from_utf8_lossy(&event.id.unwrap()));
    assert_eq!(std::time::Duration::from_millis(42), event.retry.unwrap());
}
