mod common;
mod model;

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn kiln_matches_model(layers in model::layers_strategy()) {
        let tars: Vec<Vec<u8>> = layers.iter().map(|ops| model::to_tar(ops)).collect();
        let expected = model::model_final(&layers);
        let kiln = common::try_convert_stack(&tars);
        prop_assert_eq!(expected.is_some(), kiln.is_ok(), "model valid = {}, kiln = {:?}", expected.is_some(), kiln.as_ref().err());
        if let (Some(state), Ok(images)) = (expected, kiln) {
            let seen = common::walk(&common::squash_all(&images));
            if let Err(e) = model::compare(&state, &seen) {
                prop_assert!(false, "{}", e);
            }
        }
    }
}
