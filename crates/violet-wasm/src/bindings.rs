//! Bindings for `violet-sdk/wit/provider.wit`.

wasmtime::component::bindgen!({
    path: "../violet-sdk/wit",
    world: "provider-world",
    imports: { default: async },
    exports: { default: async },
    with: {
        "violet:provider/network.socket": crate::socket::Socket,
        "violet:provider/files.file": crate::files::DbFile,
    },
});

pub(crate) use violet::provider::types;
