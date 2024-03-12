use arrow::record_batch::RecordBatch;
use async_trait::async_trait;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use crate::mqtt::MqttConfig;
use arroyo_formats::ser::ArrowSerializer;
use arroyo_operator::context::ArrowContext;
use arroyo_operator::operator::ArrowOperator;
use arroyo_rpc::formats::Format;
use arroyo_rpc::ControlResp;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::AsyncClient;
use rumqttc::v5::ConnectionError;

#[cfg(test)]
mod test;

pub struct MqttSinkFunc {
    pub config: MqttConfig,
    pub qos: QoS,
    pub topic: String,
    pub retain: bool,
    pub serializer: ArrowSerializer,
    pub client: Option<AsyncClient>,
    pub stopped: Arc<AtomicBool>,
}

impl MqttSinkFunc {
    pub fn new(config: MqttConfig, qos: QoS, topic: String, retain: bool, format: Format) -> Self {
        Self {
            config,
            qos,
            topic,
            retain,
            serializer: ArrowSerializer::new(format),
            client: None,
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl ArrowOperator for MqttSinkFunc {
    fn name(&self) -> String {
        format!("mqtt-producer-{}", self.topic)
    }
    async fn on_start(&mut self, ctx: &mut ArrowContext) {
        let mut attempts = 0;
        while attempts < 20 {
            match super::create_connection(&self.config, ctx.task_info.task_index) {
                Ok((client, mut eventloop)) => {
                    self.client = Some(client);
                    let stopped = self.stopped.clone();
                    tokio::spawn(async move {
                        while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
                            match eventloop.poll().await {
                                Ok(_) => (),
                                Err(err) => match err {
                                    ConnectionError::Timeout(_) => (),
                                    ConnectionError::MqttState(rumqttc::v5::StateError::Io(
                                        err,
                                    ))
                                    | ConnectionError::Io(err)
                                        if err.kind() == std::io::ErrorKind::ConnectionAborted
                                            || err.kind()
                                                == std::io::ErrorKind::ConnectionReset =>
                                    {
                                        continue;
                                    }
                                    err => {
                                        tracing::error!("Failed to poll mqtt eventloop: {:?}", err);
                                        tokio::time::sleep(Duration::from_secs(1)).await;
                                    }
                                },
                            }
                        }
                    });
                    return;
                }
                Err(e) => {
                    ctx.report_error("Failed to connect", e.to_string()).await;
                }
            };

            tokio::time::sleep(Duration::from_millis((50 * (1 << attempts)).min(5_000))).await;
            attempts -= 1;
        }

        panic!("Failed to establish connection to mqtt after 20 retries");
    }

    async fn process_batch(&mut self, batch: RecordBatch, ctx: &mut ArrowContext) {
        for v in self.serializer.serialize(&batch) {
            match self
                .client
                .as_mut()
                .unwrap()
                .publish(&self.topic, self.qos, self.retain, v)
                .await
            {
                Ok(_) => (),
                Err(e) => {
                    ctx.control_tx
                        .send(ControlResp::Error {
                            operator_id: ctx.task_info.operator_id.clone(),
                            task_index: ctx.task_info.task_index,
                            message: "Could not write to mqtt".to_string(),
                            details: format!("{:?}", e),
                        })
                        .await
                        .unwrap();

                    panic!("Could not write to mqtt: {:?}", e);
                }
            }
        }
    }
}

impl Drop for MqttSinkFunc {
    fn drop(&mut self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
