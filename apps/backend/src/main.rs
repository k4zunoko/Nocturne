use nocturne_api::ApiVersion;
use nocturne_core::NocturneCore;
use nocturne_infrastructure::InfrastructureProfile;

fn main() {
    let core = NocturneCore::new();
    println!(
        "Nocturne backend scaffold ready (api {}, profile {}, infra {})",
        ApiVersion::V1,
        core.workspace_profile(),
        InfrastructureProfile::Local
    );
}
