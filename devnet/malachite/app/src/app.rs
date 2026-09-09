//! Arkiv's local-devnet consensus adapter. Only certified blocks become canonical.
use crate::state::State;
use alloy_rpc_types_engine::{ExecutionPayloadV3, PayloadStatusEnum};
use bytes::Bytes;
use color_eyre::eyre::{self, ensure, eyre};
use malachitebft_app::engine::host::Next;
use malachitebft_app_channel::app::types::codec::Codec;
use malachitebft_app_channel::app::types::core::{Round, Validity};
use malachitebft_app_channel::app::types::sync::RawDecidedValue;
use malachitebft_app_channel::app::types::{LocallyProposedValue, ProposedValue};
use malachitebft_app_channel::{AppMsg, Channels, NetworkMsg};
use malachitebft_eth_engine::{engine::Engine, json_structures::ExecutionBlock};
use malachitebft_eth_types::{
    codec::proto::ProtobufCodec, Block, Height, TestContext, Value, ValueId,
};
use ssz::{Decode, Encode};
use std::collections::BTreeMap;
use tracing::info;

fn decode(value: &Value) -> eyre::Result<ExecutionPayloadV3> {
    ensure!(
        Value::new(value.extensions.clone()).id() == value.id(),
        "value hash mismatch"
    );
    ExecutionPayloadV3::from_ssz_bytes(&value.extensions)
        .map_err(|e| eyre!("invalid payload: {e:?}"))
}

async fn validate(
    engine: &Engine,
    value: &Value,
    height: Height,
    parent: ExecutionBlock,
) -> eyre::Result<()> {
    let payload = decode(value)?;
    let inner = &payload.payload_inner.payload_inner;
    ensure!(
        inner.block_number == height.as_u64(),
        "wrong payload height"
    );
    ensure!(
        inner.parent_hash == parent.block_hash,
        "wrong payload parent"
    );
    let block: Block = payload
        .clone()
        .try_into_block()
        .map_err(|e| eyre!("invalid block: {e:?}"))?;
    let hashes = block.body.blob_versioned_hashes_iter().copied().collect();
    let status = engine.notify_new_block(payload, hashes).await?;
    ensure!(
        status.status == PayloadStatusEnum::Valid,
        "payload not VALID: {status:?}"
    );
    Ok(())
}

async fn apply(engine: &Engine, value: &Value) -> eyre::Result<ExecutionBlock> {
    let payload = decode(value)?;
    let p = payload.payload_inner.payload_inner;
    engine.set_latest_forkchoice_state(p.block_hash).await?;
    Ok(ExecutionBlock {
        block_hash: p.block_hash,
        block_number: p.block_number,
        parent_hash: p.parent_hash,
        timestamp: p.timestamp,
        prev_randao: p.prev_randao,
    })
}

pub async fn run(
    state: &mut State,
    channels: &mut Channels<TestContext>,
    engine: Engine,
) -> eyre::Result<()> {
    let mut values: BTreeMap<ValueId, Value> = BTreeMap::new();
    while let Some(msg) = channels.consensus.recv().await {
        match msg {
            AppMsg::ConsensusReady { reply } => {
                if state.latest_block.is_some() {
                    continue;
                }
                engine.check_capabilities().await?;
                let mut head = engine
                    .eth
                    .get_block_by_number("latest")
                    .await?
                    .ok_or_else(|| eyre!("missing genesis"))?;
                // Never infer finality from an execution head alone.
                if head.block_number > 0 {
                    let saved = state
                        .get_decided_value(Height::new(head.block_number))
                        .await?
                        .ok_or_else(|| {
                            eyre!("execution head lacks local certificate; reset devnet")
                        })?;
                    ensure!(
                        decode(&saved.value)?.payload_inner.payload_inner.block_hash
                            == head.block_hash,
                        "consensus/execution mismatch"
                    );
                }
                // A crash can occur after the durable decision and before forkchoiceUpdated.
                while let Some(saved) = state
                    .get_decided_value(Height::new(head.block_number + 1))
                    .await?
                {
                    validate(&engine, &saved.value, saved.certificate.height, head).await?;
                    head = apply(&engine, &saved.value).await?;
                }
                state.current_height = Height::new(head.block_number + 1);
                state.latest_block = Some(head);
                let _ = reply.send((state.current_height, state.get_validator_set().clone()));
            }
            AppMsg::StartedRound {
                height,
                round,
                proposer,
                reply_value,
                ..
            } => {
                state.current_height = height;
                state.current_round = round;
                state.current_proposer = Some(proposer);
                let _ = reply_value.send(Vec::new());
                info!(%height, %round, %proposer, "started round");
            }
            AppMsg::GetValue {
                height,
                round,
                timeout,
                reply,
            } => {
                let parent = state.latest_block.ok_or_else(|| eyre!("missing parent"))?;
                let payload =
                    match tokio::time::timeout(timeout, engine.generate_block(&parent)).await {
                        Ok(Ok(payload)) => payload,
                        other => {
                            tracing::warn!(?other, "proposal build failed; allow round timeout");
                            continue;
                        }
                    };
                let bytes = Bytes::from(payload.as_ssz_bytes());
                let value = Value::new(bytes.clone());
                validate(&engine, &value, height, parent).await?;
                let proposal = state.propose_value(height, round, bytes.clone()).await?;
                values.insert(value.id(), value);
                let _ = reply.send(proposal.clone());
                for part in state.stream_proposal(proposal, bytes) {
                    channels
                        .network
                        .send(NetworkMsg::PublishProposalPart(part))
                        .await?;
                }
            }
            AppMsg::ReceivedProposalPart { from, part, reply } => {
                let mut proposed = state.received_proposal_part(from, part).await?;
                if let Some(p) = proposed.as_mut() {
                    let parent = state.latest_block.ok_or_else(|| eyre!("missing parent"))?;
                    if let Err(error) = validate(&engine, &p.value, p.height, parent).await {
                        tracing::warn!(%error, "reject proposal");
                        p.validity = Validity::Invalid;
                    } else {
                        values.insert(p.value.id(), p.value.clone());
                    }
                }
                let _ = reply.send(proposed);
            }
            AppMsg::Decided {
                certificate, reply, ..
            } => {
                let value = values
                    .get(&certificate.value_id)
                    .ok_or_else(|| eyre!("decided payload unavailable"))?
                    .clone();
                let parent = state.latest_block.ok_or_else(|| eyre!("missing parent"))?;
                validate(&engine, &value, certificate.height, parent).await?;
                // The value and certificate share one durable transaction in Store.
                state.persist_decision(&certificate, value.clone()).await?;
                let head = apply(&engine, &value).await?;
                info!(height = head.block_number, hash = %head.block_hash, "finalized Arkiv block");
                state.latest_block = Some(head);
                state.current_height = certificate.height.increment();
                state.current_round = Round::new(0);
                values.clear();
                let _ = reply.send(Next::Start(
                    state.current_height,
                    state.get_validator_set().clone(),
                ));
            }
            AppMsg::ProcessSyncedValue {
                height,
                round,
                proposer,
                value_bytes,
                reply,
            } => {
                let value: Value = ProtobufCodec.decode(value_bytes)?;
                let parent = state.latest_block.ok_or_else(|| eyre!("missing parent"))?;
                let validity = if validate(&engine, &value, height, parent).await.is_ok() {
                    values.insert(value.id(), value.clone());
                    Validity::Valid
                } else {
                    Validity::Invalid
                };
                let _ = reply.send(Some(ProposedValue {
                    height,
                    round,
                    valid_round: Round::Nil,
                    proposer,
                    value,
                    validity,
                }));
            }
            AppMsg::GetDecidedValue { height, reply } => {
                let saved = state.get_decided_value(height).await?;
                let raw = saved
                    .map(|d| {
                        Ok::<_, eyre::Report>(RawDecidedValue {
                            certificate: d.certificate,
                            value_bytes: ProtobufCodec.encode(&d.value)?,
                        })
                    })
                    .transpose()?;
                let _ = reply.send(raw);
            }
            AppMsg::RestreamProposal {
                height,
                round,
                value_id,
                ..
            } => {
                if let Some(value) = values.get(&value_id).cloned() {
                    let proposal = LocallyProposedValue::new(height, round, value.clone());
                    for part in state.stream_proposal(proposal, value.extensions) {
                        channels
                            .network
                            .send(NetworkMsg::PublishProposalPart(part))
                            .await?;
                    }
                }
            }
            AppMsg::GetValidatorSet { reply, .. } => {
                let _ = reply.send(Some(state.get_validator_set().clone()));
            }
            AppMsg::GetHistoryMinHeight { reply } => {
                let _ = reply.send(state.get_earliest_height().await);
            }
            AppMsg::ExtendVote { reply, .. } => {
                let _ = reply.send(None);
            }
            AppMsg::VerifyVoteExtension { reply, .. } => {
                let _ = reply.send(Ok(()));
            }
        }
    }
    Err(eyre!("consensus channel closed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use malachitebft_eth_engine::{engine_rpc::EngineRPC, ethereum_rpc::EthereumRPC};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn voting_requires_valid_execution_without_changing_fork_choice() {
        for status in ["VALID", "INVALID", "SYNCING", "ACCEPTED"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url: url::Url = format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap();
            let task = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut input = Vec::new();
                let mut buf = [0; 4096];
                loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    input.extend_from_slice(&buf[..n]);
                    if let Some(split) = input.windows(4).position(|s| s == b"\r\n\r\n") {
                        if let Ok(body) =
                            serde_json::from_slice::<serde_json::Value>(&input[split + 4..])
                        {
                            assert_eq!(body["method"], "engine_newPayloadV3");
                            break;
                        }
                    }
                }
                let body = serde_json::json!({"jsonrpc":"2.0", "id":1, "result": {"status":status, "latestValidHash":null, "validationError":null}}).to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            });
            let dir = tempfile::tempdir().unwrap();
            let jwt = dir.path().join("jwt");
            std::fs::write(&jwt, "11".repeat(32)).unwrap();
            let engine = Engine::new(
                EngineRPC::new(url.clone(), &jwt).unwrap(),
                EthereumRPC::new(url).unwrap(),
            );
            let mut block = Block::default();
            block.header.number = 1;
            block.header.base_fee_per_gas = Some(1);
            block.header.timestamp = 1;
            let payload = ExecutionPayloadV3::from_block_slow(&block);
            let value = Value::new(Bytes::from(payload.as_ssz_bytes()));
            let parent = ExecutionBlock {
                block_hash: block.header.parent_hash,
                block_number: 0,
                parent_hash: block.header.parent_hash,
                timestamp: 0,
                prev_randao: block.header.mix_hash,
            };
            let result = validate(&engine, &value, Height::new(1), parent).await;
            assert_eq!(result.is_ok(), status == "VALID", "{status}: {result:?}");
            task.await.unwrap();
        }
    }
}
