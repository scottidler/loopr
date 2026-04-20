use std::str::FromStr;

use taskstore_async::AsyncStore;

use domain::{Plan, PlanId};

use crate::error::StoreError;

pub struct PlansStore<'a> {
    inner: &'a AsyncStore,
}

impl<'a> PlansStore<'a> {
    pub(crate) fn new(inner: &'a AsyncStore) -> Self {
        Self { inner }
    }

    pub async fn create(&self, plan: Plan) -> Result<PlanId, StoreError> {
        let id_str = plan.id.as_ref().to_string();
        if self.inner.get::<Plan>(&id_str).await?.is_some() {
            return Err(StoreError::AlreadyExists {
                collection: "plans",
                id: id_str,
            });
        }
        let returned = self.inner.create(plan).await?;
        Ok(PlanId::from_str(&returned).expect("PlanId::from_str is Infallible"))
    }

    pub async fn get(&self, id: &PlanId) -> Result<Plan, StoreError> {
        match self.inner.get::<Plan>(id.as_ref()).await? {
            Some(plan) => Ok(plan),
            None => Err(StoreError::RecordNotFound {
                collection: "plans",
                id: id.to_string(),
            }),
        }
    }

    pub async fn list(&self) -> Result<Vec<Plan>, StoreError> {
        Ok(self.inner.list::<Plan>(&[]).await?)
    }
}
