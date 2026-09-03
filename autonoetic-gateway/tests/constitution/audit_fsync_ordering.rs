//! Constitution P-8.16 durability intent checks.


use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use crate::support::TestWorkspace;

#[test]
fn sqlite_has_synchronous_full() {
    let workspace = TestWorkspace::new().expect("workspace");
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("mkdir .gateway");

    let _store = GatewayStore::open(&gateway_dir).expect("store open");

    let db_path = gateway_dir.join("gateway.db");
    assert!(db_path.exists(), "gateway.db should exist");

    let conn = rusqlite::Connection::open(&db_path).expect("open gateway.db");
    let synchronous: i64 = conn
        .query_row("PRAGMA synchronous;", [], |row| row.get(0))
        .expect("read PRAGMA synchronous");

    assert_eq!(
        synchronous, 2,
        "gateway.db should use PRAGMA synchronous=FULL"
    );
}
