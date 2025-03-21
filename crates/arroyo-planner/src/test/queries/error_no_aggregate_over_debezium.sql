CREATE TABLE debezium_input (
    id int PRIMARY KEY,
    count int
) WITH (
    connector = 'kafka',
    bootstrap_servers = 'localhost:9092',
    type = 'source',
    topic = 'updating',
    format = 'debezium_json'
);

SELECT count(*) FROM debezium_input