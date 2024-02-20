use crate::queries::api_queries;
use crate::queries::api_queries::{
    CreateUdfParams, DbUdf, DeleteUdfParams, GetUdfByNameParams, GetUdfParams,
};
use crate::rest::AppState;
use crate::rest_utils::{
    authenticate, bad_request, client, internal_server_error, log_and_map, not_found,
    service_unavailable, ApiError, BearerAuth, ErrorResp,
};
use crate::{compiler_service, to_micros};
use arroyo_rpc::api_types::udfs::{GlobalUdf, UdfPost, UdfValidationResult, ValidateUdfPost};
use arroyo_rpc::api_types::GlobalUdfCollection;
use arroyo_rpc::public_ids::{generate_id, IdTypes};
use arroyo_rpc::grpc::{BuildUdfReq, UdfCrate};
use axum::extract::{Path, State};
use axum::Json;
use axum_extra::extract::WithRejection;
use cornucopia_async::Params;
use tonic::transport::Channel;
use tracing::error;
use arroyo_df::{ArroyoSchemaProvider, parse_dependencies, ParsedUdf, udfs};
use arroyo_rpc::grpc::compiler_grpc_client::CompilerGrpcClient;
use arroyo_types::{COMPILER_ADDR_ENV, COMPILER_PORT_ENV, ports, service_port};

impl Into<GlobalUdf> for DbUdf {
    fn into(self) -> GlobalUdf {
        GlobalUdf {
            id: self.pub_id,
            prefix: self.prefix,
            name: self.name,
            created_at: to_micros(self.created_at),
            definition: self.definition,
            updated_at: to_micros(self.updated_at),
            description: self.description,
        }
    }
}

/// Create a global UDF
#[utoipa::path(
    post,
    path = "/v1/udfs",
    tag = "udfs",
    request_body = UdfPost,
    responses(
        (status = 200, description = "Created UDF", body = Udf),
    ),
)]
pub async fn create_udf(
    State(state): State<AppState>,
    bearer_auth: BearerAuth,
    WithRejection(Json(req), _): WithRejection<Json<UdfPost>, ApiError>,
) -> Result<Json<GlobalUdf>, ErrorResp> {
    let mut client = client(&state.pool).await.unwrap();
    let auth_data = authenticate(&state.pool, bearer_auth).await.unwrap();

    let transaction = client.transaction().await.map_err(log_and_map)?;
    transaction
        .execute("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE", &[])
        .await
        .map_err(log_and_map)?;

    // build udf
    let check_udfs_resp = build_udf(&req.definition, true).await?;

    if check_udfs_resp.errors.len() > 0 {
        return Err(bad_request("UDF is invalid"));
    }

    let Some(udf_name) = check_udfs_resp.name else {
        // this should not be possible
        return Err(internal_server_error("UDF name not found"));
    };

    // check for duplicates
    let duplicate = api_queries::get_udf_by_name()
        .params(
            &transaction,
            &GetUdfByNameParams {
                organization_id: &auth_data.organization_id,
                name: &udf_name,
            },
        )
        .opt()
        .await
        .map_err(log_and_map)?;

    if duplicate.is_some() {
        return Err(bad_request(format!(
            "Global UDF with name {} already exists",
            &udf_name
        )));
    }

    let pub_id = generate_id(IdTypes::Udf);
    api_queries::create_udf()
        .params(
            &transaction,
            &CreateUdfParams {
                pub_id: &pub_id,
                created_by: &auth_data.user_id,
                organization_id: &auth_data.organization_id,
                prefix: &req.prefix,
                name: &udf_name,
                definition: &req.definition,
                description: &req.description.unwrap_or_default(),
            },
        )
        .await
        .map_err(log_and_map)?;

    let created_udf = api_queries::get_udf()
        .params(
            &transaction,
            &GetUdfParams {
                organization_id: &auth_data.organization_id,
                pub_id: &pub_id,
            },
        )
        .one()
        .await
        .map_err(log_and_map)?
        .into();

    transaction.commit().await.map_err(log_and_map)?;

    Ok(Json(created_udf))
}

/// Get Global UDFs
#[utoipa::path(
    get,
    path = "/v1/udfs",
    tag = "udfs",
    responses(
        (status = 200, description = "List of UDFs", body = GlobalUdfCollection),
    ),
)]
pub async fn get_udfs(
    State(state): State<AppState>,
    bearer_auth: BearerAuth,
) -> Result<Json<GlobalUdfCollection>, ErrorResp> {
    let client = client(&state.pool).await.unwrap();
    let auth_data = authenticate(&state.pool, bearer_auth).await.unwrap();

    let udfs = api_queries::get_udfs()
        .bind(&client, &auth_data.organization_id)
        .all()
        .await
        .map_err(log_and_map)?;

    Ok(Json(GlobalUdfCollection {
        data: udfs.into_iter().map(|u| u.into()).collect(),
    }))
}

/// Delete UDF
#[utoipa::path(
    delete,
    path = "/v1/udfs/{id}",
    tag = "udfs",
    params(
        ("id" = String, Path, description = "UDF id")
    ),
    responses(
        (status = 200, description = "Deleted UDF"),
    ),
)]
pub async fn delete_udf(
    State(state): State<AppState>,
    bearer_auth: BearerAuth,
    Path(udf_pub_id): Path<String>,
) -> Result<(), ErrorResp> {
    let client = client(&state.pool).await.unwrap();
    let auth_data = authenticate(&state.pool, bearer_auth).await.unwrap();

    let count = api_queries::delete_udf()
        .params(
            &client,
            &DeleteUdfParams {
                organization_id: &auth_data.organization_id,
                pub_id: &udf_pub_id,
            },
        )
        .await
        .map_err(log_and_map)?;

    if count != 1 {
        return Err(not_found("UDF"));
    }

    Ok(())
}

pub struct UdfResp {
    pub errors: Vec<String>,
    pub name: Option<String>,
    pub url: Option<String>,
}

impl From<anyhow::Error> for UdfResp {
    fn from(value: anyhow::Error) -> Self {
        Self {
            errors: vec![value.to_string()],
            name: None,
            url: None,
        }
    }
}

pub async fn build_udf(
    udf_definition: &str,
    save: bool,
) -> Result<UdfResp, ErrorResp> {
    let dependencies = match parse_dependencies(udf_definition) {
        Ok(dependencies) => dependencies,
        Err(e) => {
            return Ok(e.into());
        }
    };

    // use the ArroyoSchemaProvider to do some validation and to get the function name
    let function_name = match ParsedUdf::try_parse(udf_definition) {
        Ok(function_name) => function_name.name,
        Err(e) => return Ok(e.into()),
    };

    let cargo_toml = udfs::cargo_toml(&dependencies);

    let check_udfs_resp = match compiler_service()?
        .build_udf(BuildUdfReq {
            udf_crate: Some(UdfCrate {
                name: function_name.clone(),
                definition: udf_definition.to_string(),
                cargo_toml,
                lib_rs: match udfs::lib_rs(&function_name, udf_definition) {
                    Ok(lib_rs) => lib_rs,
                    Err(e) => return Ok(e.into()),
                },
            }),
            save,
        })
        .await
    {
        Ok(resp) => resp.into_inner(),
        Err(e) => {
            error!("compiler service failed to validate UDF: {}", e.message());
            return Err(internal_server_error(format!(
                "Failed to validate UDF: {}",
                e.message()
            )));
        }
    };

    Ok(UdfResp {
        errors: check_udfs_resp.errors,
        name: Some(function_name),
        url: check_udfs_resp.udf_path,
    })
}

/// Validate UDFs
#[utoipa::path(
    post,
    path = "/v1/udfs/validate",
    tag = "udfs",
    request_body = ValidateUdfPost,
    responses(
        (status = 200, description = "Validated query", body = UdfValidationResult),
    ),
)]
pub async fn validate_udf(
    State(state): State<AppState>,
    WithRejection(Json(req), _): WithRejection<Json<ValidateUdfPost>, ApiError>,
) -> Result<Json<UdfValidationResult>, ErrorResp> {
    let check_udfs_resp = build_udf(&req.definition, false).await?;

    Ok(Json(UdfValidationResult {
        udf_name: check_udfs_resp.name,
        errors: check_udfs_resp.errors,
    }))
}


