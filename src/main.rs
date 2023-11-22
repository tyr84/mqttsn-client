use tokio::net::UdpSocket;
use mqttsn_client::mqttsn::{MqttSnClient, MqttMessage};
use mqttsn_client::socket::TokioUdp;
use mqttsn_client::dtls::DtlsSocket;
use heapless::String;
use tokio::time::{sleep, Duration};
use log::*;

#[tokio::main]
async fn main() {
    env_logger::init();
    // let socket = TokioUdp(UdpSocket::bind("127.0.0.1:3400").await.unwrap());
    let socket = DtlsSocket::new().await.unwrap();
    let session = socket.connect("127.0.0.1:1234").await.unwrap();
    info!("DTLS connected");
    let mut mqtt_client = MqttSnClient::new(
        &String::<32>::from("test1"), session
    ).unwrap();
    mqtt_client.connect().await.unwrap();
    info!("MQTT-SN connected");
    mqtt_client.subscribe("test/recv".into()).await.unwrap();
    debug!("subscribed");
    mqtt_client.publish(
        MqttMessage::new("test/testing".into(), "blablabla".into())
    ).await.unwrap();
    debug!("published");
    loop {
        if let Some(msg) = mqtt_client.recieve().await.unwrap() {
            info!("new message");
            dbg!(&msg);
        }
        sleep(Duration::from_secs(5)).await;
    }
}
