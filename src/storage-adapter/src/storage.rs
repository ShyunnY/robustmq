// Copyright 2023 RobustMQ Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use axum::async_trait;
use common_base::error::common::CommonError;
use metadata_struct::adapter::read_config::ReadConfig;
use metadata_struct::adapter::record::Record;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

use crate::memory::MemoryStorageAdapter;

pub type ArcStorageAdapter = Arc<Box<dyn StorageAdapter + Send + Sync>>;

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct ShardInfo {
    pub namespace: String,
    pub shard_name: String,
    pub replica_num: u32,
}

#[derive(Default, Clone)]
pub struct ShardOffset {
    pub namespace: String,
    pub shard_name: String,
    pub segment_no: u32,
    pub offset: u64,
}

#[async_trait]
pub trait StorageAdapter {
    async fn create_shard(&self, shard: ShardInfo) -> Result<(), CommonError>;

    async fn list_shard(
        &self,
        namespace: String,
        shard_name: String,
    ) -> Result<Vec<ShardInfo>, CommonError>;

    async fn delete_shard(&self, namespace: String, shard_name: String) -> Result<(), CommonError>;

    async fn write(
        &self,
        namespace: String,
        shard_name: String,
        data: Record,
    ) -> Result<u64, CommonError>;

    async fn batch_write(
        &self,
        namespace: String,
        shard_name: String,
        data: Vec<Record>,
    ) -> Result<Vec<u64>, CommonError>;

    async fn read_by_offset(
        &self,
        namespace: String,
        shard_name: String,
        offset: u64,
        read_config: ReadConfig,
    ) -> Result<Vec<Record>, CommonError>;

    async fn read_by_tag(
        &self,
        namespace: String,
        shard_name: String,
        offset: u64,
        tag: String,
        read_config: ReadConfig,
    ) -> Result<Vec<Record>, CommonError>;

    async fn read_by_key(
        &self,
        namespace: String,
        shard_name: String,
        offset: u64,
        key: String,
        read_config: ReadConfig,
    ) -> Result<Vec<Record>, CommonError>;

    async fn get_offset_by_timestamp(
        &self,
        namespace: String,
        shard_name: String,
        timestamp: u64,
    ) -> Result<Option<ShardOffset>, CommonError>;

    async fn get_offset_by_group(
        &self,
        group_name: String,
    ) -> Result<Vec<ShardOffset>, CommonError>;

    async fn commit_offset(
        &self,
        group_name: String,
        namespace: String,
        offset: HashMap<String, u64>,
    ) -> Result<(), CommonError>;

    async fn close(&self) -> Result<(), CommonError>;
}

pub fn build_memory_storage_driver() -> ArcStorageAdapter {
    Arc::new(Box::new(MemoryStorageAdapter::new()))
}
