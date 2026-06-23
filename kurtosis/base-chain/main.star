# Adds the bespoke reth "base chain" below the Arkiv chain — the DA / settlement
# substrate the committer posts to later. Plain reth, no Arkiv semantics, no CL
# (--dev auto-mines). Run into the same enclave as the Arkiv chain.
#
# No remote imports here on purpose: kurtosis can't import a remote package from
# a local one, so this stays self-contained and the Arkiv chain runs separately.

def run(plan, args):
    plan.add_service(
        name = "base-chain",
        config = ServiceConfig(
            image = args.get("image", "ghcr.io/paradigmxyz/reth:v2.2.0"),
            ports = {
                "rpc": PortSpec(
                    number = 8545,
                    transport_protocol = "TCP",
                    application_protocol = "http",
                ),
            },
            cmd = [
                "node",
                "--dev",
                "--dev.block-time", args.get("block_time", "2s"),
                "--http",
                "--http.addr", "0.0.0.0",
                "--http.api", "eth,net,web3,txpool",
            ],
        ),
    )
