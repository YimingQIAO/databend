// Copyright 2020-2021 The Datafuse Authors.
//
// SPDX-License-Identifier: Apache-2.0.

use common_exception::Result;
use common_runtime::tokio;
use rand::Rng;

use crate::api::RpcService;
use crate::configs::Config;
use crate::sessions::FuseQueryContextRef;
use crate::sessions::SessionMgr;
use crate::sessions::SessionMgrRef;

/// Start services and return the random address.
pub async fn try_start_service(nums: usize) -> Result<Vec<String>> {
    let mut results = vec![];

    for _ in 0..nums {
        let (addr, _) = start_one_service().await?;
        results.push(addr.clone());
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    Ok(results)
}

// Start service and return the session manager for create his own contexts.
pub async fn try_start_service_with_session_mgr() -> Result<(String, SessionMgrRef)> {
    let (addr, mgr) = start_one_service().await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    Ok((addr, mgr))
}

// Start a cluster and return the context who has the cluster info.
pub async fn try_create_context_with_nodes(nums: usize) -> Result<FuseQueryContextRef> {
    let addrs = try_start_service(nums).await?;
    let ctx = crate::tests::try_create_context()?;
    for (i, addr) in addrs.iter().enumerate() {
        ctx.register_one_executor(format!("executor{}", i), 10, addr.clone())
            .await?;
    }
    Ok(ctx)
}

// Start a cluster and return the context who has the cluster info.
pub async fn try_create_context_with_nodes_and_priority(
    nums: usize,
    p: &[u8],
) -> Result<FuseQueryContextRef> {
    // p is the priority array of the nodes.
    // Its length of it should be nums.
    assert_eq!(nums, p.len());
    let addrs = try_start_service(nums).await?;
    let ctx = crate::tests::try_create_context()?;
    for (i, addr) in addrs.iter().enumerate() {
        ctx.register_one_executor(format!("executor{}", i), p[i], addr.clone())
            .await?;
    }
    Ok(ctx)
}

// Start one random service and get the session manager.
async fn start_one_service() -> Result<(String, SessionMgrRef)> {
    let mut rng = rand::thread_rng();
    let port: u32 = rng.gen_range(10000..11000);
    let addr = format!("127.0.0.1:{}", port);

    let mut conf = Config::default();
    conf.flight_api_address = addr.clone();

    let session_manager = SessionMgr::try_create(100)?;
    let srv = RpcService::create(conf, session_manager.clone());
    tokio::spawn(async move {
        srv.make_server().await?;
        Result::Ok(())
    });
    Ok((addr, session_manager))
}
