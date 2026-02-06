use lapin::{
    Channel, Connection, ConnectionProperties, Consumer, ExchangeKind, options::*,
    types::FieldTable,
};
use serde::de::DeserializeOwned;
use std::{collections::HashMap, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::time::timeout;

#[derive(Error, Debug)]
pub enum SubscriberError {
    #[error("Failed to connect to RabbitMQ: {0}")]
    ConnectionFailed(String),
    #[error("Failed to open channel: {0}")]
    ChannelFailed(String),
    #[error("Failed to declare exchange: {0}")]
    ExchangeDeclarationFailed(String),
    #[error("Failed to declare queue: {0}")]
    QueueDeclarationFailed(String),
    #[error("Failed to bind queue: {0}")]
    QueueBindFailed(String),
    #[error("Failed to register consumer: {0}")]
    ConsumerRegistrationFailed(String),
    #[error("Context timeout: {0}")]
    Timeout(String),
    #[error("No callback found for routing key: {0}")]
    NoCallbackFound(String),
}

/// Message represents a received RabbitMQ message
#[derive(Debug, Clone)]
pub struct Message {
    pub body: Vec<u8>,
    pub routing_key: String,
    pub exchange: String,
    pub content_type: Option<String>,
    pub timestamp: Option<u64>,
    pub delivery_tag: u64,
}

impl Message {
    /// Unmarshals the message body into the provided type
    pub fn unmarshal_to<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }
}

pub trait Callback {
    fn on_message(&self, message: &Message) -> Result<(), Box<dyn std::error::Error>>;
}

/// Subscriber represents a RabbitMQ subscriber instance
pub struct Subscriber {
    channel: Channel,
    exchange: String,
    queue: String,
    callbacks: HashMap<String, Arc<dyn Callback + Send + Sync>>,
}

impl Subscriber {
    /// Creates a new RabbitMQ subscriber instance
    pub async fn new(
        amqp_url: &str,
        exchange_name: &str,
        queue_name: &str,
    ) -> Result<Self, SubscriberError> {
        // Create connection with timeout
        let connection = timeout(
            Duration::from_secs(60),
            Connection::connect(amqp_url, ConnectionProperties::default()),
        )
        .await
        .map_err(|_| SubscriberError::Timeout("Connection timeout".to_string()))?
        .map_err(|e| SubscriberError::ConnectionFailed(e.to_string()))?;

        // Create channel
        let channel = connection
            .create_channel()
            .await
            .map_err(|e| SubscriberError::ChannelFailed(e.to_string()))?;

        // Declare exchange with specified parameters (same as publisher)
        channel
            .exchange_declare(
                exchange_name,
                ExchangeKind::Direct,
                ExchangeDeclareOptions {
                    durable: true,
                    auto_delete: false,
                    internal: false,
                    nowait: false,
                    passive: false,
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| SubscriberError::ExchangeDeclarationFailed(e.to_string()))?;

        // Declare queue with non-exclusive, durable settings
        let queue = channel
            .queue_declare(
                queue_name,
                QueueDeclareOptions {
                    durable: true,
                    exclusive: false,
                    auto_delete: false,
                    nowait: false,
                    passive: false,
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| SubscriberError::QueueDeclarationFailed(e.to_string()))?;

        Ok(Subscriber {
            channel,
            exchange: exchange_name.to_string(),
            queue: queue.name().to_string(),
            callbacks: HashMap::new(),
        })
    }

    pub fn add_callback(
        mut self,
        routing_key: &str,
        callback: Arc<dyn Callback + Send + Sync>,
    ) -> Self {
        self.callbacks.insert(routing_key.to_string(), callback);
        self
    }

    /// Starts consuming messages from the queue with the specified routing key callbacks
    pub async fn start(&self) -> Result<(), SubscriberError> {
        // Create bindings for each routing key
        for routing_key in self.callbacks.keys() {
            self.channel
                .queue_bind(
                    &self.queue,
                    &self.exchange,
                    routing_key,
                    QueueBindOptions::default(),
                    FieldTable::default(),
                )
                .await
                .map_err(|e| {
                    SubscriberError::QueueBindFailed(format!(
                        "Failed to bind queue {} to exchange {} with routing key {}: {}",
                        self.queue, self.exchange, routing_key, e
                    ))
                })?;
        }

        // Start consuming messages
        let consumer = self
            .channel
            .basic_consume(
                &self.queue,
                "",
                BasicConsumeOptions {
                    no_ack: false, // Manual ack
                    exclusive: false,
                    no_local: false,
                    nowait: false,
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| SubscriberError::ConsumerRegistrationFailed(e.to_string()))?;

        // Process messages
        self.process_messages(consumer).await;

        Ok(())
    }

    /// Processes incoming messages
    async fn process_messages(
        &self,
        consumer: Consumer,
    ) {
        let callbacks = Arc::new(self.callbacks.clone());
        let channel = self.channel.clone();

        tokio::spawn(async move {
            use futures_util::stream::StreamExt;

            let mut message_stream = consumer;
            while let Some(delivery) = message_stream.next().await {
                if let Ok(delivery) = delivery {
                    let routing_key = delivery.routing_key.clone();
                    let message = Message {
                        body: delivery.data.clone(),
                        routing_key: delivery.routing_key.clone().to_string(),
                        exchange: delivery.exchange.clone().to_string(),
                        content_type: delivery
                            .properties
                            .content_type()
                            .as_ref()
                            .map(|s| s.to_string()),
                        timestamp: delivery.properties.timestamp().as_ref().copied(),
                        delivery_tag: delivery.delivery_tag,
                    };

                    if let Some(callback) = callbacks.get(&message.routing_key) {
                        if let Err(e) = callback.on_message(&message) {
                            eprintln!("Error processing message: {}", e);
                        }
                    } else {
                        eprintln!("No callback found for routing key: {}", routing_key);
                    }

                    // Acknowledge the message
                    if let Err(e) = channel
                        .basic_ack(delivery.delivery_tag, BasicAckOptions::default())
                        .await
                    {
                        eprintln!("Failed to acknowledge message: {}", e);
                    }
                }
            }
        });
    }

    /// Checks if the subscriber is still connected
    pub fn is_connected(&self) -> bool {
        // For now, we'll assume connection is always active
        // In a real implementation, you might want to track connection state
        true
    }

    /// Returns the exchange name
    pub fn get_exchange(&self) -> &str {
        &self.exchange
    }

    /// Returns the queue name
    pub fn get_queue(&self) -> &str {
        &self.queue
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        // Note: In Rust, we can't easily implement async Drop
        // The connection and channel will be closed when they go out of scope
        // For explicit cleanup, users should call close() method
    }
}

impl Subscriber {
    /// Closes the subscriber connection and channel
    pub async fn close(self) -> Result<(), SubscriberError> {
        // Channel will be closed when dropped
        // Connection will be closed when dropped
        Ok(())
    }
}
