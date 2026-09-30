# Test fixtures

`gcp-test-key.pem` is a **test-only** RSA key. It was generated for the tests in
`src/gcp_token/tests.rs` and has never been attached to any Google service
account or used anywhere else. It is committed on purpose; it guards nothing.

- `gcp-test-key.pem`: the private key, PKCS#8 PEM (`BEGIN PRIVATE KEY`), the form
  a Google service-account JSON carries.
  `openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048`
- `gcp-test-key.rsa-public.pem`: its public key, PKCS#1 `RSAPublicKey` PEM, the
  form `ring`'s RSA verifier reads. The tests check each minted JWT's signature
  against it.
  `openssl rsa -in gcp-test-key.pem -RSAPublicKey_out`
