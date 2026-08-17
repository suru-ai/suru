use chidori::{
    build_identity,
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent, stop_server},
    protocol::{Health, LifecycleState, SERVER_SHUTDOWN_EVENT, ServerShutdown, ShutdownReason},
    server::{self, ServerConfig},
};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use tokio::time::{Duration, timeout};

mod support;

use support::{
    read_runtime_descriptor, receive_initial_state, request_server_shutdown,
    write_runtime_descriptor,
};

#[test]
fn build_identity_changes_with_executable_contents() {
    let directory = tempfile::tempdir().expect("create build identity fixture directory");
    let executable = directory.path().join("chidori-fixture");
    std::fs::write(&executable, b"first compiled executable")
        .expect("write first executable contents");
    let first = chidori::build_identity::for_executable(&executable)
        .expect("identify first executable contents");

    std::fs::write(&executable, b"rebuilt executable").expect("write rebuilt executable contents");
    let rebuilt = chidori::build_identity::for_executable(&executable)
        .expect("identify rebuilt executable contents");

    assert_ne!(first, rebuilt);
    assert!(first.starts_with(concat!(
        env!("CARGO_PKG_NAME"),
        "@",
        env!("CARGO_PKG_VERSION"),
        "+blake3:"
    )));
}

#[tokio::test]
async fn authenticated_health_describes_the_ready_server() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "health-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();

    assert!(descriptor.base_url.starts_with("http://127.0.0.1:"));
    assert_ne!(descriptor.base_url, "http://127.0.0.1:0");

    let missing_auth = client
        .get(format!("{}/health", descriptor.base_url))
        .send()
        .await
        .expect("request health without authentication");
    assert_eq!(missing_auth.status(), reqwest::StatusCode::UNAUTHORIZED);

    let wrong_auth = client
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth("wrong-token")
        .send()
        .await
        .expect("request health with incorrect authentication");
    assert_eq!(wrong_auth.status(), reqwest::StatusCode::UNAUTHORIZED);

    let health = client
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("request authenticated health")
        .error_for_status()
        .expect("authenticated health succeeds")
        .json::<Health>()
        .await
        .expect("decode health response");

    assert_eq!(health.instance_id, descriptor.instance_id);
    assert_eq!(health.pid, std::process::id());
    assert_eq!(health.lifecycle, LifecycleState::Ready);
    assert_eq!(health.protocol_version, descriptor.protocol_version);
    assert_eq!(health.build_identity, descriptor.build_identity);
    assert!(
        descriptor.build_identity.starts_with(concat!(
            env!("CARGO_PKG_NAME"),
            "@",
            env!("CARGO_PKG_VERSION"),
            "+blake3:"
        )),
        "build identity should include the package version and executable digest"
    );
    assert_ne!(
        descriptor.build_identity,
        concat!(env!("CARGO_PKG_NAME"), "@", env!("CARGO_PKG_VERSION")),
        "package version alone cannot identify executable contents"
    );
    assert_eq!(
        descriptor.build_identity,
        build_identity::for_current_executable().expect("identify current test executable")
    );

    let missing_event_auth = client
        .get(format!("{}/v1/events", descriptor.base_url))
        .send()
        .await
        .expect("request event stream without authentication");
    assert_eq!(
        missing_event_auth.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    let stop_request = ServerShutdown {
        instance_id: descriptor.instance_id,
        reason: ShutdownReason::Manual,
    };
    let missing_stop_auth = client
        .post(format!("{}/v1/server/stop", descriptor.base_url))
        .json(&stop_request)
        .send()
        .await
        .expect("request stop without authentication");
    assert_eq!(
        missing_stop_auth.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let malformed_missing_stop_auth = client
        .post(format!("{}/v1/server/stop", descriptor.base_url))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("not-json")
        .send()
        .await
        .expect("request malformed stop without authentication");
    assert_eq!(
        malformed_missing_stop_auth.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let wrong_stop_auth = client
        .post(format!("{}/v1/server/stop", descriptor.base_url))
        .bearer_auth("wrong-token")
        .json(&stop_request)
        .send()
        .await
        .expect("request stop with incorrect authentication");
    assert_eq!(wrong_stop_auth.status(), reqwest::StatusCode::UNAUTHORIZED);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let descriptor_mode = std::fs::metadata(
            ServerConfig::new(state_dir.path(), "health-test")
                .expect("configure server")
                .descriptor_path(),
        )
        .expect("read runtime descriptor metadata")
        .permissions()
        .mode()
            & 0o777;
        assert_eq!(descriptor_mode, 0o600);

        let lock_mode = std::fs::metadata(state_dir.path().join("health-test/server.lock"))
            .expect("read server lock metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(lock_mode, 0o600);

        let directory_mode = std::fs::metadata(state_dir.path().join("health-test"))
            .expect("read runtime directory metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
    }

    #[cfg(windows)]
    {
        assert_windows_current_user_only(state_dir.path().join("health-test"));
        assert_windows_current_user_only(
            ServerConfig::new(state_dir.path(), "health-test")
                .expect("configure server")
                .descriptor_path(),
        );
        assert_windows_current_user_only(state_dir.path().join("health-test/server.lock"));
    }

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn stop_refuses_a_mismatched_instance_without_affecting_the_server() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "mismatched-stop-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{}/v1/server/stop", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&ServerShutdown {
            instance_id: uuid::Uuid::new_v4(),
            reason: ShutdownReason::Manual,
        })
        .send()
        .await
        .expect("request shutdown for the wrong instance");

    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    let health = client
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("request health after rejected shutdown")
        .error_for_status()
        .expect("server remains reachable after rejected shutdown")
        .json::<Health>()
        .await
        .expect("decode health after rejected shutdown");
    assert_eq!(health.instance_id, descriptor.instance_id);
    assert_eq!(health.lifecycle, LifecycleState::Ready);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn authenticated_manual_stop_notifies_clients_and_removes_its_registration() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config = ServerConfig::new(state_dir.path(), "manual-stop-test").expect("configure server");
    let server = server::spawn(config.clone()).await.expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut managed = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "manual-stop-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    receive_initial_state(&mut managed).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{}/v1/server/stop", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&ServerShutdown {
            instance_id: descriptor.instance_id,
            reason: ShutdownReason::Manual,
        })
        .send()
        .await
        .expect("request manual shutdown");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);

    let stopping = client
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("request health during graceful shutdown")
        .error_for_status()
        .expect("health remains available during graceful shutdown")
        .json::<Health>()
        .await
        .expect("decode stopping health");
    assert_eq!(stopping.lifecycle, LifecycleState::Stopping);

    let shutdown = timeout(Duration::from_secs(1), async {
        loop {
            match managed.next().await {
                Some(ManagedEvent::ServerShutdown(shutdown)) => break shutdown,
                Some(ManagedEvent::CounterUpdated(_)) => {}
                Some(ManagedEvent::Recovering(status)) => {
                    panic!("manual shutdown triggered recovery: {status:?}")
                }
                Some(event) => panic!("expected manual shutdown intent, got {event:?}"),
                None => panic!("managed client closed before shutdown intent"),
            }
        }
    })
    .await
    .expect("managed client receives manual shutdown intent");
    assert_eq!(shutdown.instance_id, descriptor.instance_id);
    assert_eq!(shutdown.reason, ShutdownReason::Manual);
    assert!(matches!(
        timeout(Duration::from_secs(1), managed.next()).await,
        Ok(None)
    ));

    timeout(Duration::from_secs(1), async {
        while config.descriptor_path().exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stopping server removes its own registration");
    server.shutdown().await.expect("join stopped server");
}

#[tokio::test]
async fn stop_waits_for_its_target_when_the_registration_is_replaced() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config =
        ServerConfig::new(state_dir.path(), "stop-target-wait-test").expect("configure server");
    let server = server::spawn(config.clone()).await.expect("spawn server");
    let descriptor = server.descriptor().clone();
    let stop_config = ManagedClientConfig::new(state_dir.path(), "stop-target-wait-test")
        .expect("configure stop client");
    let stopping = tokio::spawn(async move { stop_server(&stop_config).await });
    let client = reqwest::Client::new();

    timeout(Duration::from_secs(1), async {
        loop {
            let health = client
                .get(format!("{}/health", descriptor.base_url))
                .bearer_auth(&descriptor.token)
                .send()
                .await
                .expect("request health while stop begins")
                .error_for_status()
                .expect("health remains available while stop begins")
                .json::<Health>()
                .await
                .expect("decode health while stop begins");
            if health.lifecycle == LifecycleState::Stopping {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("server enters stopping before registration replacement");

    let mut replacement = descriptor.clone();
    replacement.instance_id = uuid::Uuid::new_v4();
    write_runtime_descriptor(config.descriptor_path(), &replacement);

    stopping
        .await
        .expect("stop task does not panic")
        .expect("stop waits for the original instance");
    assert!(
        client
            .get(format!("{}/health", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .is_err(),
        "stop returned while the authenticated target was still reachable"
    );
    let remaining = read_runtime_descriptor(config.descriptor_path());
    assert_eq!(remaining.instance_id, replacement.instance_id);

    server.shutdown().await.expect("join stopped server");
}

#[cfg(windows)]
fn assert_windows_current_user_only(path: impl AsRef<std::path::Path>) {
    use std::{mem, os::windows::ffi::OsStrExt, ptr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
            GetSecurityDescriptorControl, GetTokenInformation, SE_DACL_PROTECTED, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    struct LocalSecurityDescriptor(*mut std::ffi::c_void);
    impl Drop for LocalSecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: GetNamedSecurityInfoW allocated this descriptor with LocalAlloc.
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: OpenProcessToken returned this owned handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    let path = path.as_ref();
    let path_utf16 = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: path is NUL-terminated and the requested output pointers are writable.
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_utf16.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(status, ERROR_SUCCESS, "read DACL for {path:?}");
    assert!(!descriptor.is_null(), "security descriptor for {path:?}");
    assert!(!dacl.is_null(), "DACL for {path:?}");
    let descriptor = LocalSecurityDescriptor(descriptor);

    let mut control = 0;
    let mut revision = 0;
    // SAFETY: descriptor is live and both output pointers are writable.
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) },
        0,
        "read DACL control flags for {path:?}"
    );
    assert_ne!(
        control & SE_DACL_PROTECTED,
        0,
        "DACL inherits broader access for {path:?}"
    );

    let mut acl_info = ACL_SIZE_INFORMATION::default();
    // SAFETY: dacl is owned by the live descriptor and acl_info is writable.
    assert_ne!(
        unsafe {
            GetAclInformation(
                dacl,
                ptr::from_mut(&mut acl_info).cast(),
                mem::size_of_val(&acl_info) as u32,
                AclSizeInformation,
            )
        },
        0,
        "inspect DACL for {path:?}"
    );
    assert_eq!(
        acl_info.AceCount, 1,
        "DACL grants access to more than the current user for {path:?}"
    );
    let mut ace = ptr::null_mut();
    // SAFETY: the DACL reports one ACE and ace points to writable storage.
    assert_ne!(
        unsafe { GetAce(dacl, 0, &mut ace) },
        0,
        "read DACL entry for {path:?}"
    );
    // SAFETY: the sole ACE was created as an ACCESS_ALLOWED_ACE by the runtime SDDL.
    let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    assert_eq!(
        ace.Header.AceType, ACCESS_ALLOWED_ACE_TYPE,
        "sole DACL entry does not grant access for {path:?}"
    );

    let mut token = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle and token is writable.
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0,
        "open current process token"
    );
    let token = OwnedHandle(token);
    let mut required_bytes = 0;
    // SAFETY: a null buffer with length zero is the documented size-query operation.
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required_bytes);
    }
    assert!(required_bytes > 0, "size current user token data");
    let mut token_data = vec![0usize; (required_bytes as usize).div_ceil(mem::size_of::<usize>())];
    // SAFETY: token_data is aligned, writable, and at least required_bytes long.
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                token_data.as_mut_ptr().cast(),
                required_bytes,
                &mut required_bytes,
            )
        },
        0,
        "read current user token data"
    );
    // SAFETY: GetTokenInformation initialized the buffer with TOKEN_USER.
    let token_user = unsafe { &*token_data.as_ptr().cast::<TOKEN_USER>() };
    let ace_sid = ptr::addr_of!(ace.SidStart).cast_mut().cast();
    // SAFETY: both pointers refer to valid SIDs owned by live allocations.
    assert_ne!(
        unsafe { EqualSid(ace_sid, token_user.User.Sid) },
        0,
        "DACL is not restricted to the current user for {path:?}"
    );
}

#[tokio::test]
async fn server_recovers_from_an_abandoned_partial_publication() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "partial-publication-test";
    let runtime_dir = state_dir.path().join(channel);
    std::fs::create_dir_all(&runtime_dir).expect("create runtime directory");
    std::fs::write(
        runtime_dir.join(format!("runtime.{}.tmp", std::process::id())),
        b"{\"base_url\":",
    )
    .expect("seed abandoned partial publication");

    let server =
        server::spawn(ServerConfig::new(state_dir.path(), channel).expect("configure server"))
            .await
            .expect("recover from abandoned partial publication");

    let published = read_runtime_descriptor(runtime_dir.join("runtime.json"));
    assert_eq!(published.instance_id, server.descriptor().instance_id);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn server_holds_the_channel_election_lock_for_its_lifetime() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "lifetime-lock-test";
    let config = ServerConfig::new(state_dir.path(), channel).expect("configure server");
    let first = server::spawn(config.clone())
        .await
        .expect("spawn election winner");
    let first_token = first.descriptor().token.clone();

    let contender = server::spawn(config.clone())
        .await
        .err()
        .expect("a second server cannot own the same channel");
    assert!(
        contender
            .to_string()
            .contains("another server already owns")
    );

    first.shutdown().await.expect("shut down election winner");
    assert!(
        !config.descriptor_path().exists(),
        "the election winner removes its own descriptor"
    );
    let successor = server::spawn(config)
        .await
        .expect("elect a successor after the winner exits");
    assert_ne!(successor.descriptor().token, first_token);
    successor.shutdown().await.expect("shut down successor");
}

#[tokio::test]
async fn shutdown_does_not_remove_a_descriptor_owned_by_another_instance() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config =
        ServerConfig::new(state_dir.path(), "ownership-cleanup-test").expect("configure server");
    let server = server::spawn(config.clone()).await.expect("spawn server");
    let mut replacement = server.descriptor().clone();
    replacement.instance_id = uuid::Uuid::new_v4();
    write_runtime_descriptor(config.descriptor_path(), &replacement);

    server.shutdown().await.expect("shut down original server");

    let remaining = read_runtime_descriptor(config.descriptor_path());
    assert_eq!(remaining.instance_id, replacement.instance_id);
}

#[tokio::test]
async fn descriptor_replacement_never_exposes_a_partial_publication() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };

    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config =
        ServerConfig::new(state_dir.path(), "atomic-publication-test").expect("configure server");
    let runtime_dir = state_dir.path().join("atomic-publication-test");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime directory");
    let descriptor_path = config.descriptor_path();
    let stale = chidori::protocol::RuntimeDescriptor {
        base_url: "http://127.0.0.1:9".to_owned(),
        token: "stale-token".to_owned(),
        instance_id: uuid::Uuid::new_v4(),
        pid: 1,
        protocol_version: chidori::protocol::PROTOCOL_VERSION,
        build_identity: "stale-build".to_owned(),
    };
    write_runtime_descriptor(&descriptor_path, &stale);

    let ready = Arc::new(Barrier::new(2));
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let descriptor_path = descriptor_path.clone();
        let ready = ready.clone();
        let stop = stop.clone();
        std::thread::spawn(move || -> Result<Vec<uuid::Uuid>, String> {
            let mut observed = Vec::new();
            let first: chidori::protocol::RuntimeDescriptor = serde_json::from_reader(
                std::fs::File::open(&descriptor_path)
                    .map_err(|error| format!("open initial descriptor: {error}"))?,
            )
            .map_err(|error| format!("decode initial descriptor: {error}"))?;
            observed.push(first.instance_id);
            ready.wait();
            while !stop.load(Ordering::SeqCst) {
                let descriptor: chidori::protocol::RuntimeDescriptor = serde_json::from_reader(
                    std::fs::File::open(&descriptor_path)
                        .map_err(|error| format!("open descriptor during publication: {error}"))?,
                )
                .map_err(|error| format!("decode descriptor during publication: {error}"))?;
                observed.push(descriptor.instance_id);
                std::thread::yield_now();
            }
            Ok(observed)
        })
    };
    ready.wait();

    let server = server::spawn(config)
        .await
        .expect("replace stale descriptor");
    tokio::time::sleep(Duration::from_millis(25)).await;
    stop.store(true, Ordering::SeqCst);
    let observed = reader
        .join()
        .expect("descriptor reader does not panic")
        .expect("every observed descriptor is complete");

    assert!(observed.contains(&stale.instance_id));
    assert!(observed.contains(&server.descriptor().instance_id));
    assert!(observed.iter().all(|instance_id| {
        *instance_id == stale.instance_id || *instance_id == server.descriptor().instance_id
    }));

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn managed_client_receives_snapshot_before_absolute_counter_updates() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "events-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "events-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");

    let (identity, snapshot) = receive_initial_state(&mut client).await;
    assert_eq!(identity.instance_id, descriptor.instance_id);
    assert_eq!(identity.pid, descriptor.pid);
    assert_eq!(snapshot.instance_id, descriptor.instance_id);
    assert_eq!(snapshot.value, 0);
    assert_eq!(snapshot.revision, 0);

    let update = timeout(Duration::from_secs(2), client.next())
        .await
        .expect("counter update arrives")
        .expect("managed client remains open");
    let ManagedEvent::CounterUpdated(update) = update else {
        panic!("expected counter update event, got {update:?}");
    };
    assert_eq!(update.value, 1);
    assert_eq!(update.revision, 1);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn stalled_subscriber_does_not_delay_the_counter_or_a_healthy_subscriber() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "stalled-subscriber-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let stalled_response = reqwest::Client::new()
        .get(format!("{}/v1/events", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open stalled subscriber")
        .error_for_status()
        .expect("stalled subscriber authenticates");
    let mut healthy = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "stalled-subscriber-test")
            .expect("configure healthy managed client"),
    )
    .await
    .expect("connect healthy managed client");
    let (_, snapshot) = receive_initial_state(&mut healthy).await;

    let first = timeout(Duration::from_secs(2), healthy.next())
        .await
        .expect("healthy subscriber receives first update")
        .expect("healthy subscriber remains connected");
    let ManagedEvent::CounterUpdated(first) = first else {
        panic!("expected first counter update, got {first:?}");
    };
    let second = timeout(Duration::from_secs(2), healthy.next())
        .await
        .expect("healthy subscriber receives second update")
        .expect("healthy subscriber remains connected");
    let ManagedEvent::CounterUpdated(second) = second else {
        panic!("expected second counter update, got {second:?}");
    };

    assert!(first.revision > snapshot.revision);
    assert_eq!(second.revision, first.revision + 1);
    assert_eq!(first.value, first.revision);
    assert_eq!(second.value, second.revision);

    drop(stalled_response);
    drop(healthy);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn sse_keepalive_comments_are_periodic_and_revision_neutral() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "keepalive-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let response = reqwest::Client::new()
        .get(format!("{}/v1/events", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open event stream")
        .error_for_status()
        .expect("event stream authenticates");
    let mut chunks = response.bytes_stream();
    let mut raw = Vec::new();

    timeout(Duration::from_secs(12), async {
        loop {
            let chunk = chunks
                .next()
                .await
                .expect("event stream remains open")
                .expect("read event stream bytes");
            raw.extend_from_slice(&chunk);
            let text = String::from_utf8_lossy(&raw);
            let Some(comment_position) = text.find(": keep-alive\n\n") else {
                continue;
            };
            if text[comment_position + ": keep-alive\n\n".len()..].contains("id: ") {
                break;
            }
        }
    })
    .await
    .expect("keepalive comment arrives independently of counter updates");

    let text = String::from_utf8(raw).expect("SSE response is UTF-8");
    let records = text.split("\n\n").collect::<Vec<_>>();
    let comment_index = records
        .iter()
        .position(|record| *record == ": keep-alive")
        .expect("keepalive is an SSE comment without event metadata");
    let revisions = records
        .iter()
        .filter_map(|record| {
            record
                .lines()
                .find_map(|line| line.strip_prefix("id: "))
                .map(|revision| revision.parse::<u64>().expect("revision ID is numeric"))
        })
        .collect::<Vec<_>>();
    assert!(
        records[..comment_index]
            .iter()
            .any(|record| record.contains("id: "))
    );
    assert!(
        records[comment_index + 1..]
            .iter()
            .any(|record| record.contains("id: "))
    );
    assert!(
        revisions.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "keepalives must not consume counter revisions: {revisions:?}"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn graceful_server_shutdown_emits_intent_without_starting_crash_recovery() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "shutdown-intent-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let instance_id = server.descriptor().instance_id;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "shutdown-intent-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    receive_initial_state(&mut client).await;

    let observe_shutdown = async {
        loop {
            match timeout(Duration::from_secs(1), client.next())
                .await
                .expect("shutdown intent arrives")
            {
                Some(ManagedEvent::ServerShutdown(shutdown)) => {
                    assert_eq!(shutdown.instance_id, instance_id);
                    assert_eq!(shutdown.reason, ShutdownReason::Manual);
                    break;
                }
                Some(ManagedEvent::CounterUpdated(_)) => {}
                Some(ManagedEvent::Recovering(status)) => {
                    panic!("graceful shutdown triggered recovery: {status:?}")
                }
                Some(event) => panic!("expected shutdown intent, got {event:?}"),
                None => panic!("managed client closed without shutdown intent"),
            }
        }
        assert!(matches!(
            timeout(Duration::from_secs(1), client.next()).await,
            Ok(None)
        ));
    };
    let (shutdown_result, ()) = tokio::join!(server.shutdown(), observe_shutdown);
    shutdown_result.expect("shut down server gracefully");
}

#[tokio::test]
async fn authenticated_replacement_stop_emits_replacement_intent() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "replacement-intent-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let http = reqwest::Client::new();
    let response = http
        .get(format!("{}/v1/events", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open authenticated event stream")
        .error_for_status()
        .expect("event stream opens");
    let mut events = response.bytes_stream().eventsource();
    events
        .next()
        .await
        .expect("snapshot arrives")
        .expect("snapshot is valid");

    let response = request_server_shutdown(&descriptor, ShutdownReason::Replacement).await;
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);

    let event = timeout(Duration::from_secs(1), async {
        loop {
            let event = events
                .next()
                .await
                .expect("event stream remains open")
                .expect("shutdown event is valid");
            if event.event == SERVER_SHUTDOWN_EVENT {
                break event;
            }
        }
    })
    .await
    .expect("replacement intent arrives before transport closure");
    let shutdown: ServerShutdown =
        serde_json::from_str(&event.data).expect("decode replacement intent");
    assert_eq!(shutdown.instance_id, descriptor.instance_id);
    assert_eq!(shutdown.reason, ShutdownReason::Replacement);

    server
        .run_until_ctrl_c()
        .await
        .expect("join replaced server");
}

#[tokio::test]
async fn counter_advances_without_connected_clients() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "idle-counter-test").expect("configure server"),
    )
    .await
    .expect("spawn server");

    tokio::time::sleep(Duration::from_millis(1_100)).await;

    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "idle-counter-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect after server has run without clients");
    let (_, snapshot) = receive_initial_state(&mut client).await;
    assert!(snapshot.value >= 1);
    assert_eq!(snapshot.value, snapshot.revision);

    drop(client);
    server.shutdown().await.expect("shut down server");
}
