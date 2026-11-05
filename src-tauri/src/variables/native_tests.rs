use super::credentials::{CredentialStore, NativeCredentials};

/// Runs the actual platform credential adapter, never the test credential map.
#[test]
#[ignore = "Requires an unlocked native credential store; run explicitly during native QA"]
fn native_protected_storage_round_trip_and_cleanup() {
    let reference = uuid::Uuid::new_v4().to_string();
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = NativeCredentials.remove(&self.0);
        }
    }
    let _cleanup = Cleanup(reference.clone());
    let disposable = format!("disposable-{}", uuid::Uuid::new_v4());
    NativeCredentials
        .set(&reference, &disposable)
        .expect("native write failed");
    let readback = NativeCredentials
        .get(&reference)
        .expect("native read failed");
    assert!(readback == disposable, "native value readback differed");
    NativeCredentials
        .remove(&reference)
        .expect("native cleanup failed");
    assert!(
        NativeCredentials.get(&reference).is_err(),
        "native credential still readable after cleanup"
    );
}
