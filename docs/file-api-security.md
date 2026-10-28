# File API availability

`POST /v1/files` is disabled and returns HTTP 501 (`error.type=not_implemented`)
after the normal authentication middleware. It does not read the upload body or
access object storage. Configuring object storage, using an unrestricted inference
key, or authenticating as an administrator does not enable it. There is no
configuration bypass. File listing, download, and deletion remain unimplemented.

The previous upload endpoint returned random file IDs without a durable mapping
to object keys. It cannot safely be exposed while ownership, admission, and
lifecycle are absent. Do not treat an inference-model grant as a storage grant.

A future file API must ship these guarantees together before uploads are enabled:

- Explicit file-operation grants, enforced independently of model permissions.
- Per-file bytes and aggregate storage limits; request and concurrency admission
  that remains atomic across replicas and cancellation/failure paths.
- Tenant/owner-scoped durable metadata mapping file IDs to object keys.
- Authorized retrieval/deletion and bounded retention with failure recovery.
- Disposable-store tests for granted/denied keys, quota exhaustion, concurrent
  admission, ownership isolation, deletion, and retention.

META-857's immediate remediation disables the unsafe endpoint. This change does
not implement or claim the future grants/quota/lifecycle capabilities above.

Existing objects are not deleted by this change. Old file IDs cannot locate
objects, and content-addressed blobs may also be referenced by trace/image
attachments. Operators must inventory and classify existing data and apply their
bucket retention policy before any deletion. A bucket-wide purge is unsafe.
Internal trace/image attachment storage is a separate path; this change does not
make retention or quota guarantees about that path.
