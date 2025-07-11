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

use std::net::SocketAddr;
use std::sync::Arc;

use common_base::tools::{now_mills, now_second};
use delay_message::DelayMessageManager;
use grpc_clients::pool::ClientPool;
use protocol::mqtt::common::{
    qos, Connect, ConnectProperties, ConnectReturnCode, Disconnect, DisconnectProperties,
    DisconnectReasonCode, LastWill, LastWillProperties, Login, MqttPacket, MqttProtocol, PingReq,
    PubAck, PubAckProperties, PubAckReason, PubComp, PubCompProperties, PubCompReason, PubRec,
    PubRecProperties, PubRecReason, PubRel, PubRelProperties, Publish, PublishProperties, QoS,
    Subscribe, SubscribeProperties, SubscribeReasonCode, UnsubAckReason, Unsubscribe,
    UnsubscribeProperties,
};
use schema_register::schema::SchemaRegisterManager;
use storage_adapter::storage::ArcStorageAdapter;
use tracing::{error, warn};

use super::connection::{disconnect_connection, is_delete_session};
use super::delay_message::{decode_delay_topic, is_delay_topic};
use super::offline_message::save_message;
use super::response::build_pub_ack_fail;
use super::retain::{is_new_sub, try_send_retain_message};
use super::sub_auto::try_auto_subscribe;
use super::subscribe::save_subscribe;
use super::unsubscribe::remove_subscribe;
use crate::common::pkid_storage::{pkid_delete, pkid_exists, pkid_save};
use crate::handler::cache::{
    CacheManager, ConnectionLiveTime, QosAckPackageData, QosAckPackageType,
};
use crate::handler::connection::{build_connection, get_client_id};
use crate::handler::flapping_detect::check_flapping_detect;
use crate::handler::last_will::save_last_will_message;
use crate::handler::response::{
    build_puback, build_pubrec, response_packet_mqtt_connect_fail,
    response_packet_mqtt_connect_success, response_packet_mqtt_distinct_by_reason,
    response_packet_mqtt_ping_resp, response_packet_mqtt_pubcomp_fail,
    response_packet_mqtt_pubcomp_success, response_packet_mqtt_suback,
    response_packet_mqtt_unsuback,
};
use crate::handler::session::{build_session, save_session};
use crate::handler::topic::{get_topic_name, try_init_topic};
use crate::handler::validator::{
    connect_validator, publish_validator, subscribe_validator, un_subscribe_validator,
};
use crate::observability::system_topic::event::{
    st_report_connected_event, st_report_disconnected_event, st_report_subscribed_event,
    st_report_unsubscribed_event,
};
use crate::security::AuthDriver;
use crate::server::common::connection_manager::ConnectionManager;
use crate::subscribe::common::min_qos;
use crate::subscribe::manager::SubscribeManager;

#[derive(Clone)]
pub struct MqttService {
    protocol: MqttProtocol,
    cache_manager: Arc<CacheManager>,
    connection_manager: Arc<ConnectionManager>,
    message_storage_adapter: ArcStorageAdapter,
    delay_message_manager: Arc<DelayMessageManager>,
    subscribe_manager: Arc<SubscribeManager>,
    schema_manager: Arc<SchemaRegisterManager>,
    client_pool: Arc<ClientPool>,
    auth_driver: Arc<AuthDriver>,
}

impl MqttService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        protocol: MqttProtocol,
        cache_manager: Arc<CacheManager>,
        connection_manager: Arc<ConnectionManager>,
        message_storage_adapter: ArcStorageAdapter,
        delay_message_manager: Arc<DelayMessageManager>,
        subscribe_manager: Arc<SubscribeManager>,
        schema_manager: Arc<SchemaRegisterManager>,
        client_pool: Arc<ClientPool>,
        auth_driver: Arc<AuthDriver>,
    ) -> Self {
        MqttService {
            protocol,
            cache_manager,
            connection_manager,
            message_storage_adapter,
            delay_message_manager,
            subscribe_manager,
            client_pool,
            auth_driver,
            schema_manager,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        &mut self,
        connect_id: u64,
        connect: &Connect,
        connect_properties: &Option<ConnectProperties>,
        last_will: &Option<LastWill>,
        last_will_properties: &Option<LastWillProperties>,
        login: &Option<Login>,
        addr: &SocketAddr,
    ) -> MqttPacket {
        let cluster = self.cache_manager.get_cluster_config();

        // connect params validator
        if let Some(res) = connect_validator(
            &self.protocol,
            &cluster,
            connect,
            connect_properties,
            last_will,
            last_will_properties,
            login,
        ) {
            return res;
        }

        // blacklist check
        let (client_id, new_client_id) = get_client_id(&connect.client_id);
        let connection = build_connection(
            connect_id,
            client_id.clone(),
            &cluster,
            connect,
            connect_properties,
            addr,
        );

        if self.auth_driver.auth_connect_check(&connection).await {
            return response_packet_mqtt_connect_fail(
                &self.protocol,
                ConnectReturnCode::Banned,
                connect_properties,
                None,
            );
        }

        // login check
        match self
            .auth_driver
            .auth_login_check(login, connect_properties, addr)
            .await
        {
            Ok(flag) => {
                if !flag {
                    return response_packet_mqtt_connect_fail(
                        &self.protocol,
                        ConnectReturnCode::NotAuthorized,
                        connect_properties,
                        None,
                    );
                }
            }
            Err(e) => {
                return response_packet_mqtt_connect_fail(
                    &self.protocol,
                    ConnectReturnCode::UnspecifiedError,
                    connect_properties,
                    Some(e.to_string()),
                );
            }
        }

        // flapping detect check
        if cluster.flapping_detect.enable {
            check_flapping_detect(connect.client_id.clone(), &self.cache_manager);
        }

        let (session, new_session) = match build_session(
            connect_id,
            client_id.clone(),
            connect,
            connect_properties,
            last_will,
            last_will_properties,
            &self.client_pool,
            &self.cache_manager,
        )
        .await
        {
            Ok(data) => data,
            Err(e) => {
                return response_packet_mqtt_connect_fail(
                    &self.protocol,
                    ConnectReturnCode::MalformedPacket,
                    connect_properties,
                    Some(e.to_string()),
                );
            }
        };

        if let Err(e) = save_session(
            connect_id,
            session.clone(),
            new_session,
            client_id.clone(),
            &self.client_pool,
        )
        .await
        {
            return response_packet_mqtt_connect_fail(
                &self.protocol,
                ConnectReturnCode::MalformedPacket,
                connect_properties,
                Some(e.to_string()),
            );
        }

        if let Err(e) = save_last_will_message(
            client_id.clone(),
            last_will,
            last_will_properties,
            &self.client_pool,
        )
        .await
        {
            return response_packet_mqtt_connect_fail(
                &self.protocol,
                ConnectReturnCode::UnspecifiedError,
                connect_properties,
                Some(e.to_string()),
            );
        }

        if let Err(e) = try_auto_subscribe(
            client_id.clone(),
            login,
            &self.protocol,
            &self.client_pool,
            &self.cache_manager,
            &self.subscribe_manager,
        )
        .await
        {
            return response_packet_mqtt_connect_fail(
                &self.protocol,
                ConnectReturnCode::UnspecifiedError,
                connect_properties,
                Some(e.to_string()),
            );
        }

        let live_time = ConnectionLiveTime {
            protocol: self.protocol.clone(),
            keep_live: connection.keep_alive as u16,
            heartbeat: now_second(),
        };
        self.cache_manager
            .report_heartbeat(client_id.clone(), live_time);

        self.cache_manager.add_session(&client_id, &session);
        self.cache_manager
            .add_connection(connect_id, connection.clone());
        st_report_connected_event(
            &self.message_storage_adapter,
            &self.cache_manager,
            &self.client_pool,
            &session,
            &connection,
            connect_id,
            &self.connection_manager,
        )
        .await;

        response_packet_mqtt_connect_success(
            &self.protocol,
            &cluster,
            client_id,
            new_client_id,
            session.session_expiry as u32,
            new_session,
            connection.keep_alive,
            connect_properties,
        )
    }

    pub async fn publish(
        &self,
        connect_id: u64,
        publish: &Publish,
        publish_properties: &Option<PublishProperties>,
    ) -> Option<MqttPacket> {
        let connection = if let Some(se) = self.cache_manager.get_connection(connect_id) {
            se.clone()
        } else {
            return Some(response_packet_mqtt_distinct_by_reason(
                &self.protocol,
                Some(DisconnectReasonCode::MaximumConnectTime),
            ));
        };

        if let Some(pkg) = publish_validator(
            &self.protocol,
            &self.cache_manager,
            &self.client_pool,
            &connection,
            publish,
            publish_properties,
        )
        .await
        {
            if publish.qos == QoS::AtMostOnce {
                return None;
            } else {
                return Some(pkg);
            }
        }

        let is_pub_ack = publish.qos != QoS::ExactlyOnce;

        let mut topic_name = match get_topic_name(
            &self.cache_manager,
            connect_id,
            publish,
            publish_properties,
        )
        .await
        {
            Ok(topic_name) => topic_name,
            Err(e) => {
                return Some(build_pub_ack_fail(
                    &self.protocol,
                    &connection,
                    publish.p_kid,
                    Some(e.to_string()),
                    is_pub_ack,
                ))
            }
        };

        let mut delay_info = if is_delay_topic(&topic_name) {
            match decode_delay_topic(&topic_name) {
                Ok(data) => {
                    topic_name = data.target_topic_name.clone();
                    Some(data)
                }
                Err(e) => {
                    return Some(build_pub_ack_fail(
                        &self.protocol,
                        &connection,
                        publish.p_kid,
                        Some(e.to_string()),
                        is_pub_ack,
                    ))
                }
            }
        } else {
            None
        };

        if !self
            .auth_driver
            .auth_publish_check(&connection, &topic_name, publish.retain, publish.qos)
            .await
        {
            if is_pub_ack {
                return Some(build_puback(
                    &self.protocol,
                    &connection,
                    publish.p_kid,
                    PubAckReason::NotAuthorized,
                    None,
                    Vec::new(),
                ));
            } else {
                return Some(build_pubrec(
                    &self.protocol,
                    &connection,
                    publish.p_kid,
                    PubRecReason::NotAuthorized,
                    None,
                    Vec::new(),
                ));
            }
        }

        let topic = match try_init_topic(
            &topic_name,
            &self.cache_manager,
            &self.message_storage_adapter,
            &self.client_pool,
        )
        .await
        {
            Ok(tp) => tp,
            Err(e) => {
                return Some(build_pub_ack_fail(
                    &self.protocol,
                    &connection,
                    publish.p_kid,
                    Some(e.to_string()),
                    is_pub_ack,
                ))
            }
        };

        if delay_info.is_some() {
            let mut new_delay_info = delay_info.unwrap();
            new_delay_info.tagget_shard_name = Some(topic.topic_id.clone());
            delay_info = Some(new_delay_info);
        }

        if self.schema_manager.is_check_schema(&topic_name) {
            if let Err(e) = self.schema_manager.validate(&topic_name, &publish.payload) {
                return Some(build_pub_ack_fail(
                    &self.protocol,
                    &connection,
                    publish.p_kid,
                    Some(e.to_string()),
                    is_pub_ack,
                ));
            }
        }

        let client_id = connection.client_id.clone();

        // Persisting stores message data
        let offset = match save_message(
            &self.message_storage_adapter,
            &self.delay_message_manager,
            &self.cache_manager,
            &self.client_pool,
            publish,
            publish_properties,
            &self.subscribe_manager,
            &client_id,
            &topic,
            &delay_info,
        )
        .await
        {
            Ok(da) => {
                format!("{da:?}")
            }
            Err(e) => {
                return Some(build_pub_ack_fail(
                    &self.protocol,
                    &connection,
                    publish.p_kid,
                    Some(e.to_string()),
                    is_pub_ack,
                ))
            }
        };

        let user_properties: Vec<(String, String)> = vec![("offset".to_string(), offset)];

        self.cache_manager
            .add_topic_alias(connect_id, &topic_name, publish_properties);

        match publish.qos {
            QoS::AtMostOnce => None,
            QoS::AtLeastOnce => Some(build_puback(
                &self.protocol,
                &connection,
                publish.p_kid,
                PubAckReason::Success,
                None,
                user_properties,
            )),
            QoS::ExactlyOnce => {
                if let Err(e) = pkid_save(
                    &self.cache_manager,
                    &self.client_pool,
                    &client_id,
                    publish.p_kid,
                )
                .await
                {
                    return Some(build_pub_ack_fail(
                        &self.protocol,
                        &connection,
                        publish.p_kid,
                        Some(e.to_string()),
                        is_pub_ack,
                    ));
                }

                Some(build_pubrec(
                    &self.protocol,
                    &connection,
                    publish.p_kid,
                    PubRecReason::Success,
                    None,
                    user_properties,
                ))
            }
        }
    }

    pub async fn publish_ack(
        &self,
        connect_id: u64,
        pub_ack: &PubAck,
        _: &Option<PubAckProperties>,
    ) -> Option<MqttPacket> {
        if let Some(conn) = self.cache_manager.get_connection(connect_id) {
            let client_id = conn.client_id.clone();
            let pkid = pub_ack.pkid;
            if let Some(data) = self
                .cache_manager
                .pkid_metadata
                .get_ack_packet(&client_id, pkid)
            {
                if let Err(e) = data.sx.send(QosAckPackageData {
                    ack_type: QosAckPackageType::PubAck,
                    pkid: pub_ack.pkid,
                }) {
                    error!(
                            "send puback to channel fail, error message:{}, send data time: {}, recv ack time:{}, client_id: {}, pkid: {}, connect_id:{}",
                            e,data.create_time, now_mills(), conn.client_id, pub_ack.pkid, connect_id
                        );
                }
            }
        }

        None
    }

    pub async fn publish_rec(
        &self,
        connect_id: u64,
        pub_rec: &PubRec,
        _: &Option<PubRecProperties>,
    ) -> Option<MqttPacket> {
        if let Some(conn) = self.cache_manager.get_connection(connect_id) {
            let client_id = conn.client_id.clone();
            let pkid = pub_rec.pkid;
            if let Some(data) = self
                .cache_manager
                .pkid_metadata
                .get_ack_packet(&client_id, pkid)
            {
                if let Err(e) = data.sx.send(QosAckPackageData {
                    ack_type: QosAckPackageType::PubRec,
                    pkid: pub_rec.pkid,
                }) {
                    error!("send pubrec to channel fail, error message:{}, send data time: {}, recv rec time:{}, client_id: {}, pkid: {}, connect_id:{}",
                        e,data.create_time, now_mills(), client_id, pub_rec.pkid, connect_id);
                }
            }
        }

        None
    }

    pub async fn publish_comp(
        &self,
        connect_id: u64,
        pub_comp: &PubComp,
        _: &Option<PubCompProperties>,
    ) -> Option<MqttPacket> {
        if let Some(conn) = self.cache_manager.get_connection(connect_id) {
            let client_id = conn.client_id.clone();
            let pkid = pub_comp.pkid;
            if let Some(data) = self
                .cache_manager
                .pkid_metadata
                .get_ack_packet(&client_id, pkid)
            {
                if let Err(e) = data.sx.send(QosAckPackageData {
                    ack_type: QosAckPackageType::PubComp,
                    pkid: pub_comp.pkid,
                }) {
                    error!(
                            "send pubcomp to channel fail, error message:{}, send data time: {}, recv comp time:{}, client_id: {}, pkid: {}, connect_id:{}",
                            e,data.create_time, now_mills(), client_id, pub_comp.pkid, connect_id
                        );
                }
            }
        }
        None
    }

    pub async fn publish_rel(
        &self,
        connect_id: u64,
        pub_rel: &PubRel,
        _: &Option<PubRelProperties>,
    ) -> MqttPacket {
        let connection = if let Some(se) = self.cache_manager.get_connection(connect_id) {
            se.clone()
        } else {
            return response_packet_mqtt_distinct_by_reason(
                &self.protocol,
                Some(DisconnectReasonCode::MaximumConnectTime),
            );
        };

        let client_id = connection.client_id.clone();

        match pkid_exists(
            &self.cache_manager,
            &self.client_pool,
            &client_id,
            pub_rel.pkid,
        )
        .await
        {
            Ok(res) => {
                if !res {
                    return response_packet_mqtt_pubcomp_fail(
                        &self.protocol,
                        &connection,
                        pub_rel.pkid,
                        PubCompReason::PacketIdentifierNotFound,
                        None,
                    );
                }
            }
            Err(e) => {
                return response_packet_mqtt_pubcomp_fail(
                    &self.protocol,
                    &connection,
                    pub_rel.pkid,
                    PubCompReason::PacketIdentifierNotFound,
                    Some(e.to_string()),
                );
            }
        };

        match pkid_delete(
            &self.cache_manager,
            &self.client_pool,
            &client_id,
            pub_rel.pkid,
        )
        .await
        {
            Ok(()) => {
                connection.recv_qos_message_decr();
            }
            Err(e) => {
                return response_packet_mqtt_pubcomp_fail(
                    &self.protocol,
                    &connection,
                    pub_rel.pkid,
                    PubCompReason::PacketIdentifierNotFound,
                    Some(e.to_string()),
                );
            }
        }

        connection.recv_qos_message_decr();

        response_packet_mqtt_pubcomp_success(&self.protocol, pub_rel.pkid)
    }

    pub async fn subscribe(
        &self,
        connect_id: u64,
        subscribe: &Subscribe,
        subscribe_properties: &Option<SubscribeProperties>,
    ) -> MqttPacket {
        let connection = if let Some(se) = self.cache_manager.get_connection(connect_id) {
            se.clone()
        } else {
            return response_packet_mqtt_distinct_by_reason(
                &self.protocol,
                Some(DisconnectReasonCode::MaximumConnectTime),
            );
        };

        if let Some(packet) = subscribe_validator(
            &self.protocol,
            &self.auth_driver,
            &self.cache_manager,
            &self.subscribe_manager,
            &connection,
            subscribe,
        )
        .await
        {
            return packet;
        }

        let new_subs = is_new_sub(&connection.client_id, subscribe, &self.subscribe_manager).await;

        if let Err(e) = save_subscribe(
            &connection.client_id,
            &self.protocol,
            &self.client_pool,
            &self.cache_manager,
            &self.subscribe_manager,
            subscribe,
            subscribe_properties,
        )
        .await
        {
            return response_packet_mqtt_suback(
                &self.protocol,
                &connection,
                subscribe.packet_identifier,
                vec![SubscribeReasonCode::Unspecified],
                Some(e.to_string()),
            );
        }

        st_report_subscribed_event(
            &self.message_storage_adapter,
            &self.cache_manager,
            &self.client_pool,
            &connection,
            connect_id,
            &self.connection_manager,
            subscribe,
        )
        .await;

        try_send_retain_message(
            self.protocol.clone(),
            connection.client_id.clone(),
            subscribe.clone(),
            subscribe_properties.clone(),
            self.client_pool.clone(),
            self.cache_manager.clone(),
            self.connection_manager.clone(),
            new_subs,
        )
        .await;

        let mut return_codes: Vec<SubscribeReasonCode> = Vec::new();
        let cluster_qos = self
            .cache_manager
            .get_cluster_config()
            .mqtt_protocol_config
            .max_qos;
        for filter in subscribe.filters.clone() {
            match min_qos(qos(cluster_qos).unwrap(), filter.qos) {
                QoS::AtMostOnce => {
                    return_codes.push(SubscribeReasonCode::QoS0);
                }
                QoS::AtLeastOnce => {
                    return_codes.push(SubscribeReasonCode::QoS1);
                }
                QoS::ExactlyOnce => {
                    return_codes.push(SubscribeReasonCode::QoS2);
                }
            }
        }
        response_packet_mqtt_suback(
            &self.protocol,
            &connection,
            subscribe.packet_identifier,
            return_codes,
            None,
        )
    }

    pub async fn ping(&self, connect_id: u64, _: &PingReq) -> MqttPacket {
        let connection = if let Some(se) = self.cache_manager.get_connection(connect_id) {
            se.clone()
        } else {
            return response_packet_mqtt_distinct_by_reason(
                &self.protocol,
                Some(DisconnectReasonCode::MaximumConnectTime),
            );
        };

        let live_time = ConnectionLiveTime {
            protocol: self.protocol.clone(),
            keep_live: connection.keep_alive as u16,
            heartbeat: now_second(),
        };
        self.cache_manager
            .report_heartbeat(connection.client_id, live_time);
        response_packet_mqtt_ping_resp()
    }

    pub async fn un_subscribe(
        &self,
        connect_id: u64,
        un_subscribe: &Unsubscribe,
        _: &Option<UnsubscribeProperties>,
    ) -> MqttPacket {
        let connection = if let Some(se) = self.cache_manager.get_connection(connect_id) {
            se.clone()
        } else {
            return response_packet_mqtt_distinct_by_reason(
                &self.protocol,
                Some(DisconnectReasonCode::MaximumConnectTime),
            );
        };

        if let Some(packet) = un_subscribe_validator(
            &connection.client_id,
            &self.subscribe_manager,
            &connection,
            un_subscribe,
        )
        .await
        {
            return packet;
        }

        if let Err(e) = remove_subscribe(
            &connection.client_id,
            un_subscribe,
            &self.client_pool,
            &self.subscribe_manager,
        )
        .await
        {
            return response_packet_mqtt_unsuback(
                &connection,
                un_subscribe.pkid,
                vec![UnsubAckReason::UnspecifiedError],
                Some(e.to_string()),
            );
        }

        st_report_unsubscribed_event(
            &self.message_storage_adapter,
            &self.cache_manager,
            &self.client_pool,
            &connection,
            connect_id,
            &self.connection_manager,
            un_subscribe,
        )
        .await;

        response_packet_mqtt_unsuback(
            &connection,
            un_subscribe.pkid,
            vec![UnsubAckReason::Success],
            None,
        )
    }

    pub async fn disconnect(
        &self,
        connect_id: u64,
        disconnect: &Disconnect,
        disconnect_properties: &Option<DisconnectProperties>,
    ) -> Option<MqttPacket> {
        let connection = if let Some(se) = self.cache_manager.connection_info.get(&connect_id) {
            se.clone()
        } else {
            return None;
        };

        if let Some(session) = self.cache_manager.get_session_info(&connection.client_id) {
            st_report_disconnected_event(
                &self.message_storage_adapter,
                &self.cache_manager,
                &self.client_pool,
                &session,
                &connection,
                connect_id,
                &self.connection_manager,
                disconnect.reason_code,
            )
            .await;
        }

        let delete_session = if let Some(properties) = disconnect_properties {
            is_delete_session(&properties.user_properties)
        } else {
            false
        };

        if let Err(e) = disconnect_connection(
            &connection.client_id,
            connect_id,
            &self.cache_manager,
            &self.client_pool,
            &self.connection_manager,
            &self.subscribe_manager,
            delete_session,
        )
        .await
        {
            warn!("disconnect connection failed, {}", e.to_string());
        }

        None
    }
}
