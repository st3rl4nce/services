use {
    anyhow::Result,
    futures::StreamExt,
    lapin::{options::*, types::FieldTable, BasicProperties, Channel, Connection, Consumer},
    rand::Rng,
    reqwest::Url,
    serde::{Deserialize, Serialize},
    std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
            Mutex,
        },
        time::Duration,
    },
    tokio::sync::oneshot,
};

type Callbacks = Mutex<HashMap<u64, oneshot::Sender<Response>>>;

#[derive(Debug)]
pub struct RabbitMQWrapper {
    next_id: AtomicU64,
    request_channel: Channel,
    request_channel_id: String,
    response_channel_id: String,
    callbacks: Arc<Callbacks>,
}

type Headers = HashMap<String, Vec<String>>;

#[derive(Debug, Serialize)]
struct Request {
    body: Vec<u8>,
    url: Url,
    headers: Headers,
    method: String,
}

impl From<reqwest::Request> for Request {
    fn from(request: reqwest::Request) -> Self {
        let mut headers = Headers::default();
        for (key, value) in request.headers() {
            // This will ignore header values that include invisble ASCII characters.
            headers.entry(key.to_string()).or_default().extend(value.to_str().map(String::from));
        }
        Self {
            body: request.body().map(|b| b.as_bytes().unwrap().to_owned()).unwrap_or_default(),
            url: request.url().to_owned(),
            method: request.method().to_string(),
            headers: Default::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Response {
    pub status: u16,
    pub body: serde_json::Value
}

impl RabbitMQWrapper {
    pub async fn default() -> Result<Self> {
        Self::try_new(&"amqp://localhost:5672".parse().unwrap(), "work".into()).await
    }

    /// Implements HTTP over RabbitMQ.
    pub async fn send(
        &self,
        request: reqwest::Request,
    ) -> Result<Response> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        tracing::debug!(id, "send request to rabbitmq");
        let timeout = request.timeout().map(|d| d.clone()).unwrap_or_else(|| Duration::MAX);
        let body = serde_json::to_string(&Request::from(request)).unwrap();
        let (tx, rx) = oneshot::channel();
        self.callbacks.lock().unwrap().insert(id, tx);

        let await_response = async {
            self.request_channel
                .basic_publish(
                    "",
                    &self.request_channel_id,
                    Default::default(),
                    body.as_bytes(),
                    BasicProperties::default()
                        .with_reply_to(self.response_channel_id.clone().into())
                        .with_correlation_id(format!("{id}").into())
                        .with_expiration(format!("{}", timeout.as_millis()).into()),
                )
                .await
                .unwrap();
            rx.await.unwrap()
        };
        match tokio::time::timeout(timeout.into(), await_response).await {
            Ok(response) => Ok(response),
            Err(_) => {
                self.callbacks.lock().unwrap().remove(&id);
                Err(anyhow::anyhow!("solver took too long to respond"))
            }
        }
    }

    pub async fn try_new(rabbit_url: &Url, request_channel_id: String) -> Result<Self> {
        let id = rand::thread_rng().gen::<u64>();
        let response_channel_id = format!("solver_{id}");

        let conn = Connection::connect(rabbit_url.as_str(), Default::default()).await?;

        // Registers the queue we want to send requests to with the message broker.
        let request_channel = conn.create_channel().await?;
        request_channel
            .queue_declare(
                &response_channel_id,
                QueueDeclareOptions {
                    // Hardens message queue against crashes/restarts.
                    durable: true,
                    // TODO check which additional flags we also want
                    ..Default::default()
                },
                // TODO find out what that is
                FieldTable::default(),
            )
            .await?;

        let response_channel = conn.create_channel().await?;
        let responses = response_channel
            .basic_consume(
                &response_channel_id,
                &response_channel_id,
                Default::default(),
                Default::default(),
            )
            .await?;

        let callbacks = Arc::new(Default::default());
        // TODO add tracing info span
        tokio::spawn(Self::receiver_task(responses, Arc::clone(&callbacks)));

        Ok(Self {
            next_id: Default::default(),
            callbacks,
            response_channel_id,
            request_channel_id,
            request_channel,
        })
    }

    /// Spawns task that is responsible for receiving the computed responses and
    /// informing the waiting futures about it.
    async fn receiver_task(mut responses: Consumer, callbacks: Arc<Callbacks>) {
        while let Some(Ok(response)) = responses.next().await {
            let id = match response.properties.correlation_id().as_ref().map(|id| id.as_str().parse()) {
                Some(Ok(id)) => id,
                Some(Err(err)) => {
                    tracing::error!(?err, "failed to parse rabbitmq correlation_id");
                    continue;
                }
                None => {
                    tracing::error!("rabbitmq response has no correlation_id");
                    continue;
                }
            };
            let response: Response = match serde_json::from_slice(&response.data) {
                Err(err) => {
                    tracing::error!(?err, "failed to parse rabbitmq response data");
                    Response {
                        status: 500,
                        body: serde_json::Value::String("failed to parse rabbitmq response".into())
                    }
                }
                Ok(response) => response,
            };
            tracing::debug!(id, "received response from rabbitmq");

            // Don't match on the function call immediately to avoid extending the critical
            // section unnecessarily.
            let sender = callbacks.lock().unwrap().remove(&id);
            if let Some(sender) = sender {
                // This can fail if the solver was too slow to respond and the wrapper already
                // dropped the receiver.
                let _ = sender.send(response);
            }
        }
    }
}
