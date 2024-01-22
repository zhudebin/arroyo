use std::{collections::HashMap, sync::Arc, time::Duration};

use arrow_array::types::IntervalMonthDayNanoType;
use arrow_schema::Schema;
use arroyo_datastream::WindowType;

use datafusion::{
    execution::{
        context::{SessionConfig, SessionState},
        runtime_env::RuntimeEnv,
    },
    physical_planner::{DefaultPhysicalPlanner, PhysicalPlanner},
};
use datafusion_execution::runtime_env::RuntimeConfig;
use petgraph::{graph::DiGraph, visit::Topo};

use tracing::info;

use crate::{
    physical::{ArroyoMemExec, ArroyoPhysicalExtensionCodec, DecodingContext, EmptyRegistry},
    schemas::add_timestamp_field_arrow,
    DataFusionEdge, QueryToGraphVisitor,
};
use crate::{tables::Table, ArroyoSchemaProvider, CompiledSql};
use anyhow::{anyhow, bail, Context, Result};
use arroyo_datastream::logical::{
    LogicalEdge, LogicalEdgeType, LogicalGraph, LogicalNode, LogicalProgram, OperatorName,
};
use arroyo_rpc::grpc::api::{KeyPlanOperator, ValuePlanOperator, Window, WindowAggregateOperator};
use arroyo_rpc::{
    grpc::api::{self, TumblingWindowAggregateOperator},
    ArroyoSchema,
};
use datafusion_common::{DFSchemaRef, ScalarValue};
use datafusion_expr::{expr::ScalarFunction, BuiltinScalarFunction, Expr, LogicalPlan};
use datafusion_proto::{
    physical_plan::AsExecutionPlan,
    protobuf::{
        physical_plan_node::PhysicalPlanType, AggregateMode, PhysicalExprNode, PhysicalPlanNode,
    },
};
use petgraph::prelude::EdgeRef;
use petgraph::Direction;
use prost::Message;

pub(crate) struct Planner {
    schema_provider: ArroyoSchemaProvider,
    planner: DefaultPhysicalPlanner,
    session_state: SessionState,
}

impl Planner {
    pub fn new(schema_provider: ArroyoSchemaProvider) -> Self {
        let planner = DefaultPhysicalPlanner::default();
        let mut config = SessionConfig::new();
        config
            .options_mut()
            .optimizer
            .enable_round_robin_repartition = false;
        config.options_mut().optimizer.repartition_aggregations = false;
        let session_state =
            SessionState::new_with_config_rt(config, Arc::new(RuntimeEnv::default()))
                .with_physical_optimizer_rules(vec![]);

        Self {
            schema_provider,
            planner,
            session_state,
        }
    }

    pub(crate) async fn get_arrow_program(
        &self,
        rewriter: QueryToGraphVisitor,
    ) -> Result<CompiledSql> {
        let mut topo = Topo::new(&rewriter.local_logical_plan_graph);
        let mut program_graph: LogicalGraph = DiGraph::new();

        let mut node_mapping = HashMap::new();
        while let Some(node_index) = topo.next(&rewriter.local_logical_plan_graph) {
            let logical_extension = rewriter
                .local_logical_plan_graph
                .node_weight(node_index)
                .unwrap();

            let new_node = match logical_extension {
                crate::LogicalPlanExtension::TableScan(logical_plan) => {
                    let LogicalPlan::TableScan(table_scan) = logical_plan else {
                        panic!("expected table scan")
                    };

                    let table_name = table_scan.table_name.to_string();
                    let source = self
                        .schema_provider
                        .get_table(&table_name)
                        .ok_or_else(|| anyhow!("table {} not found", table_scan.table_name))?;

                    let Table::ConnectorTable(cn) = source else {
                        panic!("expect connector table")
                    };

                    let sql_source = cn.as_sql_source()?;
                    let source_index = program_graph.add_node(LogicalNode {
                        operator_id: format!("source_{}", program_graph.node_count()),
                        description: sql_source.source.config.description.clone(),
                        operator_name: OperatorName::ConnectorSource,
                        operator_config: api::ConnectorOp::from(sql_source.source.config)
                            .encode_to_vec(),
                        parallelism: 1,
                    });

                    let watermark_index = program_graph.add_node(LogicalNode {
                        operator_id: format!("watermark_{}", program_graph.node_count()),
                        description: "watermark".to_string(),
                        operator_name: OperatorName::Watermark,
                        parallelism: 1,
                        operator_config: api::PeriodicWatermark {
                            period_micros: 1_000_000,
                            max_lateness_micros: 0,
                            idle_time_micros: None,
                        }
                        .encode_to_vec(),
                    });

                    let mut edge: LogicalEdge = (&DataFusionEdge::new(
                        table_scan.projected_schema.clone(),
                        LogicalEdgeType::Forward,
                        vec![],
                    )
                    .unwrap())
                        .into();

                    edge.projection = table_scan.projection.clone();

                    program_graph.add_edge(source_index, watermark_index, edge);

                    node_mapping.insert(node_index, watermark_index);
                    watermark_index
                }
                crate::LogicalPlanExtension::ValueCalculation(logical_plan) => {
                    let _inputs = logical_plan.inputs();
                    let physical_plan = self
                        .planner
                        .create_physical_plan(logical_plan, &self.session_state)
                        .await;

                    let physical_plan =
                        physical_plan.context("creating physical plan for value calculation")?;

                    let physical_plan_node: PhysicalPlanNode =
                        PhysicalPlanNode::try_from_physical_plan(
                            physical_plan,
                            &ArroyoPhysicalExtensionCodec::default(),
                        )?;

                    let config = ValuePlanOperator {
                        name: "tmp".into(),
                        physical_plan: physical_plan_node.encode_to_vec(),
                    };

                    let new_node_index = program_graph.add_node(LogicalNode {
                        operator_id: format!("value_{}", program_graph.node_count()),
                        description: format!("arrow_value<{}>", config.name),
                        operator_name: OperatorName::ArrowValue,
                        operator_config: config.encode_to_vec(),
                        parallelism: 1,
                    });

                    node_mapping.insert(node_index, new_node_index);

                    new_node_index
                }
                crate::LogicalPlanExtension::KeyCalculation {
                    projection: logical_plan,
                    key_columns,
                } => {
                    info!("logical plan for key calculation:\n{:?}", logical_plan);
                    info!("input schema: {:?}", logical_plan.schema());
                    let physical_plan = self
                        .planner
                        .create_physical_plan(logical_plan, &self.session_state)
                        .await;

                    let physical_plan = physical_plan.context("creating physical plan")?;

                    println!("physical plan {:#?}", physical_plan);
                    let physical_plan_node: PhysicalPlanNode =
                        PhysicalPlanNode::try_from_physical_plan(
                            physical_plan,
                            &ArroyoPhysicalExtensionCodec::default(),
                        )?;
                    let config = KeyPlanOperator {
                        name: "tmp".into(),
                        physical_plan: physical_plan_node.encode_to_vec(),
                        key_fields: key_columns.iter().map(|column| (*column) as u64).collect(),
                    };

                    let new_node_index = program_graph.add_node(LogicalNode {
                        operator_id: format!("key_{}", program_graph.node_count()),
                        operator_name: OperatorName::ArrowKey,
                        operator_config: config.encode_to_vec(),
                        description: format!("ArrowKey<{}>", config.name),
                        parallelism: 1,
                    });

                    node_mapping.insert(node_index, new_node_index);

                    new_node_index
                }
                crate::LogicalPlanExtension::AggregateCalculation(aggregate) => {
                    let my_aggregate = aggregate.aggregate.clone();
                    let logical_plan = LogicalPlan::Aggregate(my_aggregate);

                    let LogicalPlan::TableScan(_table_scan) = aggregate.aggregate.input.as_ref()
                    else {
                        bail!("expected logical plan")
                    };
                    let logical_node = match &aggregate.window {
                        WindowType::Tumbling { width } => {
                            let mut logical_node = self.tumbling_window_config(aggregate).await?;
                            logical_node.operator_id = format!(
                                "{}_{}",
                                logical_node.operator_id,
                                program_graph.node_count()
                            );
                            Some(logical_node)
                        }
                        WindowType::Sliding { width: _, slide } => None,
                        WindowType::Instant => None,
                        WindowType::Session { gap: _ } => None,
                    }
                    .expect("only support tumbling windows for now");

                    let new_node_index = program_graph.add_node(logical_node);
                    node_mapping.insert(node_index, new_node_index);
                    new_node_index
                    /*

                    let physical_plan = self.planner
                        .create_physical_plan(&logical_plan, &self.session_state)
                        .await
                        .context("couldn't create physical plan for aggregate")?;

                    let physical_plan_node: PhysicalPlanNode =
                        PhysicalPlanNode::try_from_physical_plan(
                            physical_plan,
                            &ArroyoPhysicalExtensionCodec::default(),
                        )?;

                    let slide = match &aggregate.window {
                        WindowType::Tumbling { width } => Some(width),
                        WindowType::Sliding { width: _, slide } => Some(slide),
                        WindowType::Instant => bail!("instant window not yet implemented"),
                        WindowType::Session { gap: _ } => None,
                    };

                    let date_bin = slide.map(|slide| {
                        Expr::ScalarFunction(ScalarFunction {
                            func_def: datafusion_expr::ScalarFunctionDefinition::BuiltIn(
                                BuiltinScalarFunction::DateBin,
                            ),
                            args: vec![
                                Expr::Literal(ScalarValue::IntervalMonthDayNano(Some(
                                    IntervalMonthDayNanoType::make_value(
                                        0,
                                        0,
                                        slide.as_nanos() as i64,
                                    ),
                                ))),
                                Expr::Column(datafusion_common::Column {
                                    relation: None,
                                    name: "_timestamp".into(),
                                }),
                            ],
                        })
                    });
                    let binning_function = date_bin
                        .map(|date_bin| {
                            self.planner.create_physical_expr(
                                &date_bin,
                                &aggregate.aggregate.input.schema().as_ref(),
                                &aggregate.aggregate.input.schema().as_ref().into(),
                                &self.session_state,
                            )
                        })
                        .transpose()?;

                    let binning_function_proto = binning_function
                        .map(|binning_function| PhysicalExprNode::try_from(binning_function))
                        .transpose()?
                        .unwrap_or_default();
                    let input_schema: Schema = aggregate.aggregate.input.schema().as_ref().into();

                    let config = WindowAggregateOperator {
                        name: format!("windo_aggregate<{:?}>", aggregate.window),
                        physical_plan: physical_plan_node.encode_to_vec(),
                        binning_function: binning_function_proto.encode_to_vec(),
                        // unused now
                        binning_schema: vec![],
                        input_schema: serde_json::to_vec(&input_schema)?,
                        window: Some(Window {
                            window: Some(aggregate.window.clone().into()),
                        }),
                        window_field_name: aggregate.window_field.name().to_string(),
                        window_index: aggregate.window_index as u64,
                        key_fields: aggregate
                            .key_fields
                            .iter()
                            .map(|field| (*field) as u64)
                            .collect(),
                    };

                    let new_node_index = program_graph.add_node(LogicalNode {
                        operator_id: format!("aggregate_{}", program_graph.node_count()),
                        operator_name: OperatorName::ArrowAggregate,
                        operator_config: config.encode_to_vec(),
                        parallelism: 1,
                        description: config.name.clone(),
                    });

                    node_mapping.insert(node_index, new_node_index);
                    new_node_index*/
                }
                crate::LogicalPlanExtension::Sink {
                    name: _,
                    connector_op,
                } => {
                    let connector_op: api::ConnectorOp = connector_op.clone().into();
                    let sink_index = program_graph.add_node(LogicalNode {
                        operator_id: format!("sink_{}", program_graph.node_count()),
                        operator_name: OperatorName::ConnectorSink,
                        operator_config: connector_op.encode_to_vec(),
                        parallelism: 1,
                        description: connector_op.description.clone(),
                    });
                    node_mapping.insert(node_index, sink_index);
                    sink_index
                }
            };

            for edge in rewriter
                .local_logical_plan_graph
                .edges_directed(node_index, Direction::Incoming)
            {
                program_graph.add_edge(
                    *node_mapping.get(&edge.source()).unwrap(),
                    new_node,
                    edge.weight().try_into().unwrap(),
                );
            }
        }

        let program = LogicalProgram {
            graph: program_graph,
        };

        Ok(CompiledSql {
            program,
            connection_ids: vec![],
            schemas: HashMap::new(),
        })
    }

    async fn tumbling_window_config(
        &self,
        aggregate: &crate::AggregateCalculation,
    ) -> Result<LogicalNode> {
        let WindowType::Tumbling { width } = aggregate.window else {
            bail!("expected tumbling window")
        };
        let binning_function_proto =
            self.binning_function_proto(width, aggregate.aggregate.input.schema().clone())?;
        let input_schema: Schema = aggregate.aggregate.input.schema().as_ref().into();

        let input_schema = ArroyoSchema {
            schema: Arc::new(input_schema),
            timestamp_index: aggregate.aggregate.input.schema().fields().len() - 1,
            key_indices: aggregate.key_fields.clone(),
        };

        let my_aggregate = aggregate.aggregate.clone();
        let logical_plan = LogicalPlan::Aggregate(my_aggregate);

        let LogicalPlan::TableScan(_table_scan) = aggregate.aggregate.input.as_ref() else {
            bail!("expected logical plan");
        };

        let physical_plan = self
            .planner
            .create_physical_plan(&logical_plan, &self.session_state)
            .await
            .context("couldn't create physical plan for aggregate")?;

        let codec = ArroyoPhysicalExtensionCodec {
            context: DecodingContext::Planning,
        };

        let mut physical_plan_node: PhysicalPlanNode =
            PhysicalPlanNode::try_from_physical_plan(physical_plan.clone(), &codec)?;

        let PhysicalPlanType::Aggregate(mut final_aggregate_proto) = physical_plan_node
            .physical_plan_type
            .take()
            .ok_or_else(|| anyhow!("missing physical plan"))?
        else {
            bail!("expected aggregate physical plan, not {:?}", physical_plan);
        };

        let AggregateMode::Final = final_aggregate_proto.mode() else {
            bail!("expect AggregateMode to be Final so we can decompose it for checkpointing.")
        };

        // pull the input out to be computed separately for each bin.
        let partial_aggregation_plan = final_aggregate_proto
            .input
            .take()
            .expect("should have input");

        // need to convert to ExecutionPlan to get the partial schema.
        let partial_aggregation_exec_plan = partial_aggregation_plan.try_into_physical_plan(
            &EmptyRegistry {},
            &RuntimeEnv::new(RuntimeConfig::new()).unwrap(),
            &codec,
        )?;
        let partial_schema = partial_aggregation_exec_plan.schema();

        let final_input_table_provider = ArroyoMemExec {
            table_name: "partial".into(),
            schema: partial_schema.clone(),
        };

        final_aggregate_proto.input = Some(Box::new(PhysicalPlanNode::try_from_physical_plan(
            Arc::new(final_input_table_provider),
            &codec,
        )?));

        let finish_plan = PhysicalPlanNode {
            physical_plan_type: Some(PhysicalPlanType::Aggregate(final_aggregate_proto)),
        };

        let partial_schema = ArroyoSchema::new(
            add_timestamp_field_arrow(partial_schema.clone()),
            partial_schema.fields().len(),
            aggregate.key_fields.clone(),
        );
        let config = TumblingWindowAggregateOperator {
            name: format!("TumblingWindow<{:?}>", width),
            width_micros: width.as_micros() as u64,
            binning_function: binning_function_proto.encode_to_vec(),
            window_field_name: aggregate.window_field.name().to_string(),
            window_index: aggregate.window_index as u64,
            input_schema: Some(input_schema.try_into()?),
            partial_schema: Some(partial_schema.try_into()?),
            partial_aggregation_plan: partial_aggregation_plan.encode_to_vec(),
            final_aggregation_plan: finish_plan.encode_to_vec(),
        };
        Ok(LogicalNode {
            operator_id: config.name.clone(),
            description: "tumbling window".to_string(),
            operator_name: OperatorName::TumblingWindowAggregate,
            operator_config: config.encode_to_vec(),
            parallelism: 1,
        })
    }

    fn binning_function_proto(
        &self,
        duration: Duration,
        input_schema: DFSchemaRef,
    ) -> Result<PhysicalExprNode> {
        let date_bin = Expr::ScalarFunction(ScalarFunction {
            func_def: datafusion_expr::ScalarFunctionDefinition::BuiltIn(
                BuiltinScalarFunction::DateBin,
            ),
            args: vec![
                Expr::Literal(ScalarValue::IntervalMonthDayNano(Some(
                    IntervalMonthDayNanoType::make_value(0, 0, duration.as_nanos() as i64),
                ))),
                Expr::Column(datafusion_common::Column {
                    relation: None,
                    name: "_timestamp".into(),
                }),
            ],
        });

        let binning_function = self.planner.create_physical_expr(
            &date_bin,
            &input_schema,
            &input_schema.as_ref().into(),
            &self.session_state,
        )?;
        Ok(PhysicalExprNode::try_from(binning_function)?)
    }
}
