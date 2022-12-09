use async_trait::async_trait;
use error_stack::ResultExt;
use masking::Secret;

use super::{ConstructFlowSpecificData, Feature};
use crate::{
    consts,
    core::{
        errors::{self, ConnectorErrorExt, RouterResult, StorageErrorExt},
        payments::{self, helpers, transformers, PaymentData},
    },
    routes::AppState,
    scheduler::metrics,
    services,
    types::{
        self, api,
        storage::{self, enums as storage_enums},
        PaymentsAuthorizeData, PaymentsAuthorizeRouterData, PaymentsResponseData,
    },
    utils,
};

#[async_trait]
impl
    ConstructFlowSpecificData<
        api::Authorize,
        types::PaymentsAuthorizeData,
        types::PaymentsResponseData,
    > for PaymentData<api::Authorize>
{
    async fn construct_r_d<'a>(
        &self,
        state: &AppState,
        connector_id: &str,
        merchant_account: &storage::MerchantAccount,
    ) -> RouterResult<
        types::RouterData<
            api::Authorize,
            types::PaymentsAuthorizeData,
            types::PaymentsResponseData,
        >,
    > {
        let output = transformers::construct_payment_router_data::<
            api::Authorize,
            types::PaymentsAuthorizeData,
        >(state, self.clone(), connector_id, merchant_account)
        .await?;
        Ok(output.1)
    }
}

#[async_trait]
impl Feature<api::Authorize, types::PaymentsAuthorizeData>
    for types::RouterData<api::Authorize, types::PaymentsAuthorizeData, types::PaymentsResponseData>
{
    async fn decide_flows<'a>(
        self,
        state: &AppState,
        connector: api::ConnectorData,
        customer: &Option<storage::Customer>,
        payment_data: PaymentData<api::Authorize>,
        call_connector_action: payments::CallConnectorAction,
        storage_scheme: storage_enums::MerchantStorageScheme,
    ) -> (RouterResult<Self>, PaymentData<api::Authorize>)
    where
        dyn api::Connector: services::ConnectorIntegration<
            api::Authorize,
            types::PaymentsAuthorizeData,
            types::PaymentsResponseData,
        >,
    {
        let resp = self
            .decide_flow(
                state,
                connector,
                customer,
                Some(true),
                call_connector_action,
                storage_scheme,
            )
            .await;

        metrics::PAYMENT_COUNT.add(&metrics::CONTEXT, 1, &[]); // Metrics

        (resp, payment_data)
    }
}

impl PaymentsAuthorizeRouterData {
    pub async fn decide_flow<'a, 'b>(
        &'b self,
        state: &'a AppState,
        connector: api::ConnectorData,
        maybe_customer: &Option<storage::Customer>,
        confirm: Option<bool>,
        call_connector_action: payments::CallConnectorAction,
        _storage_scheme: storage_enums::MerchantStorageScheme,
    ) -> RouterResult<PaymentsAuthorizeRouterData>
    where
        dyn api::Connector + Sync: services::ConnectorIntegration<
            api::Authorize,
            PaymentsAuthorizeData,
            PaymentsResponseData,
        >,
    {
        match confirm {
            Some(true) => {
                let connector_integration: services::BoxedConnectorIntegration<
                    api::Authorize,
                    PaymentsAuthorizeData,
                    PaymentsResponseData,
                > = connector.connector.get_connector_integration();
                let mut resp = services::execute_connector_processing_step(
                    state,
                    connector_integration,
                    self,
                    call_connector_action,
                )
                .await
                .map_err(|error| error.to_payment_failed_response())?;
                match &self.request.mandate_id {
                    Some(mandate_id) => {
                        let mandate = state
                            .store
                            .find_mandate_by_merchant_id_mandate_id(
                                resp.merchant_id.as_ref(),
                                mandate_id,
                            )
                            .await
                            .change_context(errors::ApiErrorResponse::MandateNotFound)?;
                        let mandate = match mandate.mandate_type {
                            storage_enums::MandateType::SingleUse => state
                                .store
                                .update_mandate_by_merchant_id_mandate_id(
                                    &resp.merchant_id,
                                    mandate_id,
                                    storage::MandateUpdate::StatusUpdate {
                                        mandate_status: storage_enums::MandateStatus::Revoked,
                                    },
                                )
                                .await
                                .change_context(errors::ApiErrorResponse::MandateNotFound),
                            storage_enums::MandateType::MultiUse => Ok(mandate),
                        }?;

                        resp.payment_method_id = Some(mandate.payment_method_id);
                    }
                    None => {
                        if self.request.setup_future_usage.is_some() {
                            let payment_method_id = helpers::call_payment_method(
                                state,
                                &self.merchant_id,
                                Some(&self.request.payment_method_data),
                                Some(self.payment_method),
                                maybe_customer,
                            )
                            .await?
                            .payment_method_id;

                            resp.payment_method_id = Some(payment_method_id.clone());
                            if let Some(new_mandate_data) =
                                self.generate_mandate(maybe_customer, payment_method_id)
                            {
                                resp.request.mandate_id = Some(new_mandate_data.mandate_id.clone());
                                state.store.insert_mandate(new_mandate_data).await.map_err(
                                    |err| {
                                        err.to_duplicate_response(
                                            errors::ApiErrorResponse::DuplicateRefundRequest,
                                        )
                                    },
                                )?;
                            };
                        }
                    }
                }

                Ok(resp)
            }
            _ => Ok(self.clone()),
        }
    }

    fn generate_mandate(
        &self,
        customer: &Option<storage::Customer>,
        payment_method_id: String,
    ) -> Option<storage::MandateNew> {
        match (self.request.setup_mandate_details.clone(), customer) {
            (Some(data), Some(cus)) => {
                let mandate_id = utils::generate_id(consts::ID_LENGTH, "man");

                // The construction of the mandate new must be visible
                let mut new_mandate = storage::MandateNew::default();

                new_mandate
                    .set_mandate_id(mandate_id)
                    .set_customer_id(cus.customer_id.clone())
                    .set_merchant_id(self.merchant_id.clone())
                    .set_payment_method_id(payment_method_id)
                    .set_mandate_status(storage_enums::MandateStatus::Active)
                    .set_customer_ip_address(
                        data.customer_acceptance.get_ip_address().map(Secret::new),
                    )
                    .set_customer_user_agent(data.customer_acceptance.get_user_agent())
                    .set_customer_accepted_at(Some(data.customer_acceptance.get_accepted_at()));

                Some(match data.mandate_type {
                    api::MandateType::SingleUse(data) => new_mandate
                        .set_single_use_amount(Some(data.amount))
                        .set_single_use_currency(Some(data.currency))
                        .set_mandate_type(storage_enums::MandateType::SingleUse)
                        .to_owned(),

                    api::MandateType::MultiUse => new_mandate
                        .set_mandate_type(storage_enums::MandateType::MultiUse)
                        .to_owned(),
                })
            }
            (_, _) => None,
        }
    }
}
