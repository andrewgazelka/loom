# Tenant isolation

One VM per tenant is the outer blast radius; capabilities are the model inside. Not built: the VM layer, per-tenant object-store prefixes and keys, per-tenant node keys for capability MACs.

Done when: a tenant's node cannot list, read or MAC-forge another tenant's actors, shown by a test that runs two nodes with distinct keys and prefixes.
