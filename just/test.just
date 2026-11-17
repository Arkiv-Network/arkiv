
# Run doc tests
[group('test')]
test_doc test_name="":
  cargo test {{test_name}} --workspace --all-features --doc 


# Run all tests
[group('test')]
nextest test_name="":
  cargo nextest run {{test_name}} --workspace --all-features 
