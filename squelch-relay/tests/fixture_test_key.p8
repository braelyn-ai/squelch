squelch-relay TEST FIXTURE ONLY -- a throwaway ES256 (P-256) key generated for
the unit and integration tests. It signs nothing real: it is not an Apple APNs
auth key, has never been registered with any developer account, and must never
be used as one. Committing it is deliberate so the JWT tests need no setup.

-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgrSNG215X6tP2jTxm
G4/zBAQ7UAHkXRCtIW2J9kREFtShRANCAASPs6Nnya2UbXk01EtDbKkEQCknc39F
F2dzXxhJsFphewMHvSSEDMDBtey4sZaelZx/o2zL0Bco+YlaEwqV1Jdm
-----END PRIVATE KEY-----
