use oracle_scenario_a::*;
use serde_json::json;
use std::path::PathBuf;
struct Files(PathBuf);
impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn context() -> Context {
    Context {
        guild: "guild-a".into(),
        actor: "admin".into(),
        run: "run-1".into(),
    }
}
fn verify(f: &Fixture) {
    assert_eq!(f.state.channels.len(), 4);
    let category = f
        .state
        .channels
        .iter()
        .find(|c| c.kind == Kind::Category)
        .unwrap();
    assert_eq!(category.name, "Minecraft");
    assert_eq!(category.parent, None);
    for (name, kind) in [
        ("minecraft-info", Kind::Text),
        ("minecraft-chat", Kind::Text),
        ("Minecraft Voice", Kind::Voice),
    ] {
        let list: Vec<_> = f.state.channels.iter().filter(|c| c.name == name).collect();
        assert_eq!(list.len(), 1);
        let c = list[0];
        assert_eq!(c.kind, kind);
        assert_eq!(c.parent, Some(category.id.clone()));
        assert!(!c.audience["everyone"].read);
        assert!(c.audience["minecraft"].read);
        assert_eq!(c.audience["minecraft"].write, name != "minecraft-info");
        assert!(c.audience["staff"].read && c.audience["staff"].write);
        assert!(c.audience["minecraft"].connect);
    }
}
fn main() {
    let files = Files(std::env::temp_dir().join(format!("oracle-p6-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir(&files.0).unwrap();
    let mut checks = Vec::new();
    let ctx = context();
    let mut f = Fixture::fresh();
    let path = files.0.join("fresh");
    let mut store = Store::open(&path).unwrap();
    let p = plan(&ctx, &mut f, &store).unwrap();
    let receipt = apply(&ctx, &p, &mut f, &mut store).unwrap();
    assert!(receipt.verified);
    assert_eq!(receipt.created.len(), 4);
    verify(&f);
    checks.push("fresh Minecraft category and three channels with exact approved private audience");
    drop(store);
    let mut store = Store::open(&path).unwrap();
    let p = plan(&ctx, &mut f, &store).unwrap();
    let repeated = apply(&ctx, &p, &mut f, &mut store).unwrap();
    assert!(repeated.created.is_empty());
    assert_eq!(repeated.reused.len(), 4);
    assert_eq!(f.creates, 4);
    verify(&f);
    checks.push("repeat after durable store reopen is verified no-op");
    let mut existing = Fixture::fresh();
    existing.state.channels = f
        .state
        .channels
        .iter()
        .filter(|c| c.kind == Kind::Category || c.name == "minecraft-chat")
        .cloned()
        .collect();
    existing
        .state
        .channels
        .iter_mut()
        .for_each(|c| c.topic = "custom topic survives".into());
    let original = existing.state.channels.clone();
    let mut store = Store::open(&files.0.join("existing")).unwrap();
    let p = plan(&ctx, &mut existing, &store).unwrap();
    let receipt = apply(&ctx, &p, &mut existing, &mut store).unwrap();
    assert_eq!(receipt.created.len(), 2);
    assert_eq!(receipt.reused.len(), 2);
    for old in original {
        assert_eq!(
            existing.state.channels.iter().find(|c| c.id == old.id),
            Some(&old)
        );
    }
    verify(&existing);
    checks.push("pre-existing compatible category/chat reused and custom fields preserved");
    let mut hidden = Fixture::fresh();
    hidden.state.channels = f.state.channels.clone();
    hidden.hidden.insert(hidden.state.channels[0].id.clone());
    let store = Store::open(&files.0.join("hidden")).unwrap();
    assert_eq!(
        plan(&ctx, &mut hidden, &store).unwrap_err(),
        Error::VisibilityIncomplete
    );
    assert_eq!(hidden.creates, 0);
    checks.push("hidden/incomplete discovery never treats absent result as permission to create");
    for field in ["actor", "bot", "overwrites", "hierarchy", "policy"] {
        let mut f = Fixture::fresh();
        match field {
            "actor" => f.state.policy.actor_manage = false,
            "bot" => f.state.policy.bot_manage = false,
            "overwrites" => f.state.policy.bot_manage_roles = false,
            "hierarchy" => f.state.policy.minecraft_role_position = 10,
            _ => f.state.policy.allow_game_area = false,
        };
        let store = Store::open(&files.0.join(field)).unwrap();
        assert_eq!(
            plan(&ctx, &mut f, &store).unwrap_err(),
            Error::PermissionDenied
        );
        assert_eq!(f.creates, 0);
    }
    checks.push("actor/bot permissions, overwrite authority, role hierarchy and host grant deny without writes");
    let mut ambiguous = Fixture::fresh();
    let mut category = f.state.channels[0].clone();
    category.id = "duplicate".into();
    ambiguous.state.channels = vec![f.state.channels[0].clone(), category];
    let store = Store::open(&files.0.join("ambiguous")).unwrap();
    assert_eq!(
        plan(&ctx, &mut ambiguous, &store).unwrap_err(),
        Error::AmbiguousTarget
    );
    assert_eq!(ambiguous.creates, 0);
    checks.push("duplicate visible candidate categories require resolution");
    let mut stale = Fixture::fresh();
    let mut store = Store::open(&files.0.join("stale")).unwrap();
    let p = plan(&ctx, &mut stale, &store).unwrap();
    stale.state.policy.staff_role_position = 4;
    assert_eq!(
        apply(&ctx, &p, &mut stale, &mut store).unwrap_err(),
        Error::StalePlan
    );
    assert_eq!(stale.creates, 0);
    checks.push("changed snapshot rejected at apply");
    let mut foreign = Fixture::fresh();
    let mut store = Store::open(&files.0.join("foreign")).unwrap();
    let p = plan(&ctx, &mut foreign, &store).unwrap();
    for field in ["guild", "actor", "run"] {
        let mut impostor = ctx.clone();
        match field {
            "guild" => impostor.guild = "guild-b".into(),
            "actor" => impostor.actor = "other".into(),
            _ => impostor.run = "run-2".into(),
        };
        assert_eq!(
            apply(&impostor, &p, &mut foreign, &mut store).unwrap_err(),
            Error::ScopeMismatch
        );
    }
    assert_eq!(foreign.creates, 0);
    checks.push("copied plan cannot change guild, principal or run");
    let mut lost = Fixture::fresh();
    lost.lose_response_at = Some(1);
    let path = files.0.join("lost");
    let mut store = Store::open(&path).unwrap();
    let p = plan(&ctx, &mut lost, &store).unwrap();
    assert_eq!(
        apply(&ctx, &p, &mut lost, &mut store).unwrap_err(),
        Error::UnknownOutcome
    );
    assert_eq!(lost.creates, 1);
    drop(store);
    let store = Store::open(&path).unwrap();
    let mut new_run = ctx.clone();
    new_run.run = "after-restart".into();
    assert_eq!(
        plan(&new_run, &mut lost, &store).unwrap_err(),
        Error::UnknownOutcome
    );
    assert_eq!(lost.creates, 1);
    checks.push(
        "lost create response retains reservation across restart and blocks duplicate in new run",
    );
    let mut partial = Fixture::fresh();
    partial.revoke_after = Some(1);
    let mut store = Store::open(&files.0.join("partial")).unwrap();
    let p = plan(&ctx, &mut partial, &store).unwrap();
    assert_eq!(
        apply(&ctx, &p, &mut partial, &mut store).unwrap_err(),
        Error::PermissionDenied
    );
    assert_eq!(partial.creates, 1);
    assert_eq!(partial.state.channels.len(), 1);
    checks.push("permission revoked after first write stops remainder and preserves partial state");
    let mut corrupt = Fixture::fresh();
    corrupt.corrupt_write = true;
    let mut store = Store::open(&files.0.join("corrupt")).unwrap();
    let p = plan(&ctx, &mut corrupt, &store).unwrap();
    assert_eq!(
        apply(&ctx, &p, &mut corrupt, &mut store).unwrap_err(),
        Error::ReadbackMismatch
    );
    assert_eq!(corrupt.creates, 1);
    checks.push("readback mismatch prevents success even when create response looked correct");
    let mut duplicate = Fixture::fresh();
    duplicate.duplicate_write = true;
    let mut store = Store::open(&files.0.join("duplicate-race")).unwrap();
    let p = plan(&ctx, &mut duplicate, &store).unwrap();
    assert_eq!(
        apply(&ctx, &p, &mut duplicate, &mut store).unwrap_err(),
        Error::ReadbackMismatch
    );
    assert_eq!(duplicate.creates, 1);
    checks.push("external duplicate introduced during create prevents verified completion");
    let mut edited = Fixture::fresh();
    edited.edit_previous = true;
    let mut store = Store::open(&files.0.join("earlier-edit")).unwrap();
    let p = plan(&ctx, &mut edited, &store).unwrap();
    assert_eq!(
        apply(&ctx, &p, &mut edited, &mut store).unwrap_err(),
        Error::ReadbackMismatch
    );
    checks.push("final whole-plan readback detects earlier resource changed after its individual verification");
    let mut public_duplicate = Fixture::fresh();
    public_duplicate.duplicate_write = true;
    public_duplicate.duplicate_public = true;
    let mut store = Store::open(&files.0.join("public-duplicate")).unwrap();
    let p = plan(&ctx, &mut public_duplicate, &store).unwrap();
    assert_eq!(
        apply(&ctx, &p, &mut public_duplicate, &mut store).unwrap_err(),
        Error::ReadbackMismatch
    );
    checks.push("incompatible audience duplicate also blocks verified completion");
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "p6-report.json".into());
    std::fs::write(&output,serde_json::to_vec_pretty(&json!({"prototype":"P6","status":"passed","checks":checks,"preexisting_receipt":receipt,"scope":"deterministic operation/permission fixtures; live Discord remains a separate P3 gate"})).unwrap()).unwrap();
    println!("P6 passed: {output}");
}
