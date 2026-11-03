# Versioning Guide

This document presents a guide to versioning of pallet-revive's runtime API functions. Specifically, how existing
un-versioned functions can be versioned and how versioned functions can be updated. It is written in a simple way which
can be followed either by you or your agent and could even be turned into a skill if you wanted to use it that way
(although we're not versioning the runtime APIs often enough for a skill to be too useful).

Versioning an un-versioned runtime API function or updating an existing runtime API function is a simple process but
it's quite mechanical and this document describes the steps in full which need to be taken to do it.

> [!NOTE]
> All of the paths provided in this document are relative to `polkadot-sdk/substrate/frame/revive`.

## Nomenclature

This section introduces a number of terms which will be used throughout this document and defines them once for the
purpose of allowing the document to flow in a more natural way.

<table>
  <thead>
    <tr>
      <th>Term</th>
      <th>Definition</th>
    </tr>
  </thead>
  <tbody>
    <tr>
      <td><strong>Execution Type</strong></td>
      <td>
        A Rust type which is used by pallet-revive for its internal computation, storage, and usage and which should not
        be exposed through any of the runtime API functions.
      </td>
    </tr>
    <tr>
      <td><strong>Wire Type</strong></td>
      <td>
        A Rust type which is used by pallet-revive in its runtime API functions either somewhere in the input or in the
        output and never used internally in any way which isn't simple conversions.
      </td>
    </tr>
    <tr>
      <td><strong>Non-Primitive Types</strong></td>
      <td>
        This refers to the non-primitive types which can be found in the signature of runtime API functions either as
        arguments or as output. Types such as <code>H256</code>, <code>U256</code>, <code>String</code>,
        <code>Vec</code> are treated as primitive types while other types which are defined in pallet-revive are treated
        as non-primitive types. For example, when versioning the <code>eth_receipt_data</code> runtime API function, we
        treated the <code>ReceiptGasInfo</code> as a non-primitive type and treated the numeric types used within it as
        primitive types.
      </td>
    </tr>
  </tbody>
</table>

## Versioning an Unversioned Runtime API Function

Let's assume that we want to version a runtime API function called `${function-name}`.

### Setup

- Find the existing runtime API function defined in pallet-revive in `src/lib.rs` and collect the set of non-primitive
  types in its signature.

### Type Definitions

- If the `${function-name}` signature contains non-primitive types which are not already defined in the
  `pallet-revive-types` crate then define them with a `V1` postfix in either an existing module or a new file module in
  the `types/src/runtime_api/types` module. This step makes new type definitions for new wire type(s).
  - **Example:** When versioning the `block_hash` runtime API function there were no non-primitive types and therefore
    we did not need to define any new types in the `types/runtime_api/types` module.
  - **Example:** When versioning the `eth_receipt_data` runtime API function we found two primitive numeric types and a
    single non-primitive type which contained them: `ReceiptGasInfo`. Since that type was not already defined in the
    `types/runtime_api/types` we defined it in a new file module called `receipt` and named the type `ReceiptGasInfoV1`
    according to the rules here.
- If, as part of the step above, new types are defined then appropriate conversion traits need to be implemented for
  them in `pallet-revive`. These traits need to be implement right underneath their equivalent execution types.
  - If the wire type is used as an input type then add a conversion of `From<WireType> for ExecutionType`
  - If the wire type is used as an output type then add a conversion of `From<ExecutionType> for WireType`
  - If it’s being used as both then add both conversion implementations.
  - **Example:** After the `ReceiptGasInfoV1` type was added to the `pallet-revive-types` crate we added a
    `From<ReceiptGasInfo> for ReceiptGasInfoV1` implementation in the `src/evm/block_hash.rs` module underneath the type
    definition of the execution type directly. The direction of the conversion was selected in this way since
    `ReceiptGasInfoV1` is an output type. The reverse conversion was not implemented since it's never used anywhere as
    an input to a runtime API function.

### Payload Definitions

- Add a new file module in `types/src/runtime_api/payloads` which carries the same name as the runtime API function
  which is being versioned and wire it up into the `mod.rs` file. The contents of this file need to be exactly the
  following:
  - The Polkadot-sdk license header present in all of the other files present in this repo.
  - The required `use` statements needed for this file (including imports for common collections such as `Vec` from
    `alloc` since the `pallet-revive-types` crate can be compiled with `no-std`).
  - A struct with the following specifications:
    - Named `${function-name} InputPayloadV1` (we're using V1 here since this is the first version of that payload)
    - With named fields with field names matching the argument names of the existing unversioned `${function-name}`
      runtime API function. This may optionally be a no-field struct if `${function-name}` takes no arguments.
    - Derives `TypeInfo, Debug, Clone, Encode, Decode, PartialEq` with `PartialEq` being an optional derive if the types
      used for the fields can not satisfy `PartialEq`.
    - With the same fields as the unversioned runtime API function `${function-name}`.
    - With the same generics as the unversioned runtime API function `${function-name}` has in its arguments.
  - An enum with the following specifications:
    - Named `${function-name} VersionedInputPayload`
    - With the following doc comment, substituting the name of the unversioned runtime API function for
      `${function-name}`:

      ```rust
      /// The input type used when calling the `${function-name}_versioned` runtime API function. This
      /// function replaces the unversioned `${function-name}` runtime API function.
      ```

    - With a single variant named `V1` which uses tuple-fields with a single field of the type
      `${function-name} InputPayloadV1`.
    - With the following doc comment on the `V1` variant when it maps directly to one unversioned function:

      ```rust
      /// The arguments provided when calling the `${function-name}_versioned` runtime API function.
      ///
      /// When this version is provided, the function behaves identically to and returns the same output
      /// as the unversioned `${function-name}` runtime API function.
      ```

      If `V1` combines multiple unversioned functions or otherwise changes how their arguments are represented, replace
      the simple equivalence paragraph with the exact mapping from the `V1` fields or variants to each unversioned
      function and state which behavior and output each mapping preserves.
    - Derives `TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto` with `PartialEq` being an optional
      derive if the types used for the fields can not satisfy `PartialEq`.
    - With the same generics as the unversioned runtime API function `${function-name}` has in its arguments.
  - A struct with the following specifications:
    - Named `${function-name} OutputPayloadV1` (we're using V1 here since this is the first version of that payload)
    - With named fields matching the intended return type's semantic meaning of the unversioned runtime API function
      `${function-name}`. This may optionally be a no-field struct if the runtime API function has no returns.
    - Derives `TypeInfo, Debug, Clone, Encode, Decode, PartialEq` with `PartialEq` being an optional derive if the types
      used for the fields can not satisfy `PartialEq`.
    - With the same generics as the unversioned runtime API function `${function-name}` has in its return type.
    - If the return type of `${function-name}` is an `Option<T>` then this struct needs to have a single named-field of
    type `Option<T>` (notice that we didn't just make it `T`). If the return type of `${function-name}` is
    `Result<T, E>` then this struct needs to have a single named-field of type `T`.
    <!--
    TODO: The above needs to change once/if we decide to version the error types but it's currently something that we do
    not do.
    -->
  - An enum with the following specifications:
    - Named `${function-name} VersionedOutputPayload`
    - With the following doc comment, substituting the name of the unversioned runtime API function for
      `${function-name}`:

      ```rust
      /// The output type returned when calling the `${function-name}_versioned` runtime API function.
      /// This function replaces the unversioned `${function-name}` runtime API function.
      ```

    - With a single variant named `V1` which uses tuple-fields with a single field of the type
      `${function-name} OutputPayloadV1`.
    - With the following doc comment on the `V1` variant when it maps directly to one unversioned function:

      ```rust
      /// The output returned when calling the `${function-name}_versioned` runtime API function with `V1`
      /// arguments.
      ///
      /// This output is identical to the output returned by the unversioned `${function-name}` runtime
      /// API function.
      ```

      If `V1` combines multiple unversioned functions, replace the simple equivalence paragraph with the exact
      unversioned output associated with each input mapping.
    - Derives `TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto` with `PartialEq` being an optional
      derive if the types used for the fields can not satisfy `PartialEq`.
    - With the same generics as the unversioned runtime API function `${function-name}` has in its return type.

### Pallet Revive Execution Types

- Create a new module in `src/runtime_api` named `${function-name}` and wire it up to the `mod.rs` file. The contents of
  this file need to be exactly the following:
  - The Polkadot-sdk license header present in all of the other files present in this repo.
  - The required `use` statements needed for this file (including imports for common collections such as `Vec` from
    `alloc` since the `pallet-revive-types` crate can be compiled with `no-std`).
  - A struct with the following specifications:
    - Named `${function-name} InputPayload`.
    - With the same fields as the unversioned runtime API function `${function-name}` has as arguments but with the
      field types being the appropriate execution types.
    - With the same generics as the unversioned runtime API function `${function-name}` has in its inputs.
    - Never derive `Encode`, `Decode`, `TypeInfo`, `Serialize`, or `Deserialize` for this type since it's never meant to
      cross the wire or be serialized in any capacity.
    - **Example:** the execution type of the `trace_block` runtime API function input in
      `src/runtime_api/trace_block.rs` has two fields: a generic `Block` and the execution type `TracerType`. Notice
      that the type used in this definition is an execution type and not the `TracerTypeV1` wire type.
  - An implementation of `From<${function-name} VersionedInputPayload> for ${function-name} InputPayload`.
  - An implementation of `From<${function-name} InputPayloadVn> for ${function-name} InputPayload` for each version
    which is defined (only one in this case since this is a newly versioned runtime API)
  - A struct with the following specifications:
    - Named `${function-name} OutputPayload`.
    - With the same fields as the unversioned runtime API function `${function-name}` has as return type(s) but with the
      field types being the appropriate execution types.
    - With the same generics as the unversioned runtime API function `${function-name}` has in its return type(s).
    - Never derive `Encode`, `Decode`, `TypeInfo`, `Serialize`, or `Deserialize` for this type since it's never meant to
      cross the wire or be serialized in any capacity.
    - **Example:** the execution type of the `trace_block` runtime API function output in
      `src/runtime_api/trace_block.rs` has a single field of the type `Vec<(u32, Trace)>`. Notice that the type used in
      this definition is an execution type and not the `TraceV1` wire type.
  - An implementation of `From<${function-name} OutputPayload> for ${function-name} OutputPayloadVn` for each version
    which is defined (only one in this case since this is a newly versioned runtime API)
  - Never implement `From<${function-name} OutputPayload> for ${function-name} VersionedOutputPayload` since there is no
    way to tell what version the execution output needs to be converted into and therefore it's the responsibility of
    the caller to make that conscious decision on their own.

### Adding The New Runtime API

- Declare a new runtime API function in pallet-revive in `src/lib.rs` in the `decl_runtime_api` block with the following
  specifications:
  - Named `${function-name}_versioned`.
  - Carries a `#[api_version(2)]` attribute on it.
  - Carries no comments.
  - With a single argument called `input` of the type `${function-name} VersionedInputPayload`.
  - With a single return type `${function-name} VersionedOutputPayload`.
  - Added at the same relative position as its unversioned counter-part existed at (e.g., before function X and after
    function Y).
- Implement the new versioned runtime API function in pallet-revive in `src/lib.rs` in the
  `impl_runtime_apis_plus_revive_traits` block with the following specifications:
  - Named `${function-name}_versioned`.
  - With a single argument called `input` of the type
    `$crate::pallet_revive_types::runtime_api::${function-name} VersionedInputPayload`.
  - With a single return type `$crate::pallet_revive_types::runtime_api::${function-name} VersionedOutputPayload`.
  - First line of the function block is a `use $crate::pallet_revive_types::runtime_api::*;` to make the subsequent
    matching simpler.
  - Subsequent code in the implementation of this runtime API function looks like the following:

    ```rust
    // Getting the execution input type and a function for wrapping the output to be called at
    // the end of the execution.
    //
    // Note: the wrapper function converts the output into the same version as the input which
    // fulfills the invariant that a caller who provides a Vn input is guaranteed to get back a
    // Vn output or an error.
    let (input, output_wrapper): (
        _,
        Box<dyn Fn(FunctionNameOutputPayload) -> FunctionNameVersionedOutputPayload>,
    ) = match input {
        FunctionNameVersionedInputPayload::V1(payload) => (
            FunctionNameInputPayload::from(payload),
            Box::new(|output| FunctionNameVersionedOutputPayload::V1(output.into())),
        ),
    };

    // Some computation which is performed as part of the runtime API function.
    let output = perform_function_name(input.field1, input.field2);
    let output = FunctionNameOutputPayload { field1: output.field1 };

    // Converting the return type into the same version provided by the caller.
    output_wrapper(output)
    ```

  - In the same relative position as its unversioned counter-part existed at (e.g., before function X and after function
    Y).

### Deprecations

- Deprecate the old unversioned runtime API function `${function-name}` with a deprecation notice of
  `"Use the versioned equivalent ${function-name}_versioned if available on your runtime"`.
- Update the implementation of the old unversioned runtime API function `${function-name}` in pallet-revive `src/lib.rs`
  `impl_runtime_apis_plus_revive_traits` block such that it constructs V1 input, delegates to the new versioned runtime
  API function, then deconstructs V1 input in the following way:

  ```rust
  fn function_name(argument: $crate::ArgumentType) -> $crate::ReturnType {
      use $crate::pallet_revive_types::runtime_api::*;

      let input = FunctionNameVersionedInputPayload::from(FunctionNameInputPayloadV1 {
          argument: argument
      });
      let output = Self::function_name_versioned(input);
      FunctionNameOutputPayloadV1::try_from(output)
          .expect("v1 input must produce v1 output; qed")
          .output
  }
  ```

- If this runtime API function contained non-primitive types which needed to be defined in the `pallet-revive-types`
  crate at the beginning of this procedure then change all of the runtime API functions to use the new wire types which
  were defined.
  - **Example:** when versioning the `eth_receipt_data` runtime API function we defined a new wire-type:
    `ReceiptGasInfoV1`. Then, we changed the return type of the existing unversioned `eth_receipt_data` runtime API
    function to be the new `ReceiptGasInfoV1` therefore making the `ReceiptGasInfo` type truly internal to pallet-revive
    and unused anywhere in it's interface, not even in the older unversioned runtime API functions.
  - **Example:** when versioning the `trace_block` runtime API function we defined a number of wire-types, one of them
    was the `TraceV1` type. We replaced the return type of the unversioned `trace_block` runtime API function to be
    `TraceV1` and also replaced the return type of other runtime API functions which were not versioned in the same
    commit such as `trace_call_with_config`, `trace_call`, and `trace_tx` to use the new wire types we had defined
    (again, even though they were not versioned in that commit) in order to ensure that each wire type that we add
    completely replaces the existing execution type from the interface of pallet-revive even when it's not in the
    function we're currently versioning.

### State

At this point in the procedure, the state of the codebase should be as follows:

- The non-primitive types from the signature of `${function-name}` have been defined in the `pallet-revive-types` crate.
- The payload types for the inputs and outputs of the `${function-name}` runtime API function have been defined in the
  `pallet-revive-types` crate.
- The unversioned runtime API function `${function-name}` is deprecated with an appropriate deprecation message.
- The runtime API of pallet-revive no longer contains anywhere in its entire interface any of the non-primitive types
  which have had wire types defined for them in this procedure neither in the runtime API function we're versioning nor
  in other runtime API functions.
- The new versioned runtime API function has been declared and implemented with an `#[api_version(2)]`.
- The unversioned runtime API function delegates execution to the versioned runtime API function.
- The new runtime API function handles all versions its implemented for and guarantees that `Vn` input produces `Vn`
  output.

### ETH-RPC Integration

- Add type substitutions for all of the wire-types and the payload types added as part of this procedure to the
  `rpc/src/subxt_client.rs` file's versioning section in order to make the eth-rpc use the types we have defined rather
  than the types generated by subxt.
  - For the `${function-name} InputPayloadV1`, `${function-name} OutputPayloadV1`,
    `${function-name} VersionedInputPayload`, and `${function-name} VersionedOutputPayload` types.
  - If any non-primitive wire types were defined as part of this procedure into the `types/src/runtime_api/types` module
    then replacements for all of them must be added.
- If non-primitive wire types were defined as part of this procedure then ensure that the `pallet-revive-eth-rpc` does
  not use the old execution types in anyway.
  - **Example:** When versioning the `eth_receipt_data` runtime API function we introduced the `ReceiptGasInfoV1` wire
    type. We no longer want the eth-rpc to depend on the old execution type since it's no longer being exposed by any of
    the runtime API function we have. Therefore, we added a type substitution for the new `ReceiptGasInfoV1` and ensured
    that all of the appropriate parts of the `rpc/src/receipt_extractor.rs` used the `ReceiptGasInfoV1` wire type that
    is now being returned by pallet-revive.

### Cleanups

These are done in order to ensure that as we version runtime API functions we make execution types truly internal to
pallet-revive and ensure that they do not leak out in anyway. For all of the execution types which had wire types
defined for them as part of this procedure check:

- Does this execution type still need to be `pub` or could it be downgraded to a `pub(crate)`? If it can be downgraded
  then do it.
  - Example: when versioning the `eth_receipt_data` runtime API function we could not make the execution type
    `ReceiptGasInfo` a `pub(crate)` since it's being exposed by a public pallet function (not a runtime API function).
- Does this execution type still need its scale encoding derives or could they be removed? If they could be removed then
  remove them.
  - Example: when versioning the `eth_receipt_data` runtime API function we could not remove the scale derives from the
    `ReceiptGasInfo` type since it's being stored in storage.
  - Example: when versioning the `trace_block` runtime API function we were able to remove the scale derives from the
    `Trace` type (and all of the non-primitive types which are in this type graph) since this type is no longer returned
    from any runtime API function and is not being stored in storage.
- Does this execution type still need its serde derives or could they be removed? If they could be removed then remove
  them.
  - Example: when versioning the `trace_block` runtime API function we were able to remove the serde derives from the
    `Trace` type (and all of the non-primitive types which are in this type graph) since the new wire types handle all
    of the serde serialization and deserialization implementations.

## Updating a Versioned Runtime API Function

Let's assume that we want to update an already versioned runtime API function called `${function-name}`. Throughout this
procedure, `Vn` means the latest version of the specific type or payload being discussed and `Vn+1` means the new
version being added. The value of `n` does not need to be the same for every type or runtime API function.

### Setup

- Run `types/scripts/typegraph.sh` from the pallet-revive directory. This generates an interactive SVG in the workspace
  target directory by default. A different output path can optionally be provided as the first argument.
- Open the generated SVG in a browser and click on each wire type which needs to be updated. Clicking on a type
  highlights every type in `pallet-revive-types` which transitively contains it.
- The graph contains every historical version of each type. Group the highlighted nodes by type family and inspect the
  latest definition in each family. A highlighted historical definition reveals dependency history but does not
  independently require a version bump. A family needs a new version if it is intentionally being updated or if its
  latest definition contains a type being versioned in this procedure, either directly or through another affected
  latest definition.
- Note down the affected non-payload wire type families and payload type families from this latest containment chain.
  Each affected family is versioned exactly once from its latest definition. The versioned input and output payload
  enums are extended with a new variant rather than being versioned.
- The affected runtime API functions are the `${function-name}` function which is intentionally being updated plus every
  additional function identified by an affected payload type family. Use this complete set throughout the procedure even
  when the intended update introduces only new types or changes only primitive fields and therefore can not identify the
  function through the pre-change graph.
  - **Example:** If `CallLogV2` needed to be updated, clicking it would show that `CallTraceV2` contains it, `TraceV2`
    contains `CallTraceV2`, and the V2 output payloads of the trace runtime API functions contain `TraceV2`. We would
    define a new `CallLogV3` with the intended update, a new `CallTraceV3` which contains `CallLogV3`, a new `TraceV3`
    which contains `CallTraceV3`, and new V3 payloads for those runtime API functions. None of the existing V1 or V2
    type definitions would be changed.

### Type Definitions

- If the intended inputs or outputs introduce non-primitive types which are not already defined in the
  `pallet-revive-types` crate then define them with a `V1` postfix in either an existing module or a new file module in
  `types/src/runtime_api/types`. Wire any new file module up in the `mod.rs` file. These types will not appear in the
  type graph generated before the update because they do not exist yet.
  - **Example:** When adding V3 reporting to the `trace_block` and `trace_tx` runtime API functions, `TraceEntry` was a
    new non-primitive wire type and was therefore defined as `TraceEntryV1` even though the payloads using it were V3.
- Work outwards from each wire type family which was clicked in the graph towards the affected payload type families,
  following only the latest containment chain identified during setup.
  - For each wire type family which was clicked, define exactly one new type in the same module in
    `types/src/runtime_api/types` with the next version postfix. Use the family's latest definition as the starting
    point, apply the intended update only to the new definition, and leave all of the existing definitions unchanged.
  - For each candidate non-payload wire type family whose latest definition transitively contains a type versioned in
    the previous step, define exactly one new version of the containing type. Use the family's latest definition as the
    starting point and, only in the new definition, replace references to the contained type with the new version
    defined in this procedure. Continue this process outwards until reaching the affected payload type families.
  - If multiple historical definitions from the same family are highlighted then process that family only once. If
    containment exists only in an older definition and not in the family's latest definition then the family does not
    need a new version for that historical path.
  - Increment each type from its own latest version rather than giving every type the same postfix. For example, if
    `ContainedTypeV2` is contained by `ContainingTypeV4`, define `ContainedTypeV3` and `ContainingTypeV5`.
  - Types which are not part of the affected latest containment chain continue using their existing versions. Keep the
    derives, serde attributes, field names, generics, and other behavior of the latest version unless the intended
    update requires them to change.
- Add the appropriate conversion traits for each new wire type in `pallet-revive`. These traits need to be implemented
  right underneath their equivalent execution types.
  - If the wire type is used as an input type then add a conversion of `From<WireType> for ExecutionType`.
  - If the wire type is used as an output type then add a conversion of `From<ExecutionType> for WireType`.
  - If it's being used as both then add both conversion implementations.
  - Keep conversion implementations for all of the older wire type versions. If an execution type changes as part of the
    update then change the bodies of those older conversion implementations as needed to preserve their existing wire
    shapes and semantics. Never change the older wire type definitions or remove their conversion support.
    - **Example:** When the execution output for `trace_block` changed to hold traced and untraced entries, the existing
      V1 and V2 output conversions were updated to keep projecting only traced entries. Their wire definitions and
      behavior remained unchanged.

### Payload Definitions

- For each affected versioned runtime API function, update its existing file module in `types/src/runtime_api/payloads`
  with the following:
  - A struct with the following specifications:
    - Named `${function-name} InputPayloadVn+1`.
    - With named fields which represent the intended inputs for the new version. These fields are determined by the
      update being made and may optionally form a no-field struct if the new version takes no inputs.
    - Derives `TypeInfo, Debug, Clone, Encode, Decode, PartialEq` with `PartialEq` being an optional derive if the types
      used for the fields can not satisfy `PartialEq`.
    - Use the latest input payload version as the starting point, apply the intended additions, removals, or changes to
      the new definition, and use the new versions defined in this procedure for any affected wire types.
    - With the generics required by the intended inputs for the new version.
  - A new variant in `${function-name} VersionedInputPayload` with the following specifications:
    - Named `Vn+1` and uses tuple-fields with a single field of the type `${function-name} InputPayloadVn+1`.
    - With a doc comment which describes only the difference from `Vn`. State whether the arguments changed or remained
      unchanged and name the output version selected by an otherwise unchanged input when relevant. Do not restate what
      the runtime API function does.
    - Added after all of the existing variants without changing them.
    - Add any generics required by the new input payload to `${function-name} VersionedInputPayload` and apply the
      appropriate generics to each of its variants.
  - A struct with the following specifications:
    - Named `${function-name} OutputPayloadVn+1`.
    - With named fields which represent the intended outputs for the new version. These fields are determined by the
      update being made and may optionally form a no-field struct if the new version has no outputs.
    - Derives `TypeInfo, Debug, Clone, Encode, Decode, PartialEq` with `PartialEq` being an optional derive if the types
      used for the fields can not satisfy `PartialEq`.
    - Use the latest output payload version as the starting point, apply the intended additions, removals, or changes to
      the new definition, and use the new versions defined in this procedure for any affected wire types.
    - With the generics required by the intended outputs for the new version.
    - If the intended output of the new version is an `Option<T>` then this struct needs to have a single named-field of
    type `Option<T>` (notice that we didn't just make it `T`). If the intended output of the new version is
    `Result<T, E>` then this struct needs to have a single named-field of type `T`.
    <!--
    TODO: The above needs to change once/if we decide to version the error types but it's currently something that we do
    not do.
    -->
  - A new variant in `${function-name} VersionedOutputPayload` with the following specifications:
    - Named `Vn+1` and uses tuple-fields with a single field of the type `${function-name} OutputPayloadVn+1`.
    - With a doc comment which describes only the difference from `Vn`. Name every changed wire type or field, explain
      the meaning of each change, and identify fields or portions which were removed or remained unchanged when that
      distinction matters. Do not restate what the runtime API function does.
    - Added after all of the existing variants without changing them.
    - Add any generics required by the new output payload to `${function-name} VersionedOutputPayload` and apply the
      appropriate generics to each of its variants.
- Add both the `Vn+1` input payload and the `Vn+1` output payload as new structs without changing any of the existing
  payload structs, even if only one side contains the type which initiated the update. The unchanged side uses the same
  fields and wire types as its latest version. This is needed to fulfill the invariant that a caller who provides a
  `Vn+1` input is guaranteed to get back a `Vn+1` output or an error.

### Pallet Revive Execution Types

- For each affected versioned runtime API function, update its existing `src/runtime_api/${function-name}.rs` module
  with the following conversions:
  - Add an implementation of `From<${function-name} InputPayloadVn+1> for ${function-name} InputPayload`.
  - Add the `Vn+1` variant to the existing implementation of
    `From<${function-name} VersionedInputPayload> for ${function-name} InputPayload`.
  - Add an implementation of `From<${function-name} OutputPayload> for ${function-name} OutputPayloadVn+1`.
  - Keep conversion implementations for all of the older payload versions. If an execution input or output payload
    changes as part of the update then change the bodies of those older conversion implementations as needed to preserve
    their existing wire shapes and semantics. Never change the older wire payload definitions or remove their conversion
    support.
  - Never implement `From<${function-name} OutputPayload> for ${function-name} VersionedOutputPayload` since there is no
    way to tell what version the execution output needs to be converted into and therefore it's the responsibility of
    the caller to make that conscious decision on their own.
- If the intended update changes an execution type then make the corresponding change to that execution type before
  implementing its conversions. Keep the execution input and output payload types unversioned and using the appropriate
  execution types.

### Updating The Runtime API

- For each affected versioned runtime API function, update the implementation of `${function-name}_versioned` in
  pallet-revive in the `src/lib.rs` `impl_runtime_apis_plus_revive_traits` block by adding the new version to its
  existing match statement:

  ```rust
  FunctionNameVersionedInputPayload::VnPlus1(payload) => (
      FunctionNameInputPayload::from(payload),
      Box::new(|output| FunctionNameVersionedOutputPayload::VnPlus1(output.into())),
  ),
  ```

- Add the match arm after all of the existing variants without changing them. This converts the new wire input into the
  existing execution input and wraps the execution output in the same version as the input.
- If the new payloads require generics which are not already present on the versioned payload enums, propagate those
  generics through the enums and their uses in the existing `${function-name}_versioned` declaration and implementation.
  The function must still have exactly one versioned input payload enum argument and one versioned output payload enum
  return type.
- In `version_declarations()`, update the existing `.insert("${function-name}_versioned", n)` entry to use `n+1`, the
  new latest payload version. Do this for every affected versioned runtime API function.
- Do not declare a new runtime API function or change the name of the existing `${function-name}_versioned` function or
  its `#[api_version(2)]` attribute.

### State

At this point in the procedure, the state of the codebase should be as follows:

- Any new non-primitive wire types introduced by the intended inputs or outputs have been defined in the
  `pallet-revive-types` crate with a `V1` postfix.
- Each affected non-payload wire type family from the latest containment chain has exactly one new version based on its
  latest definition and all of its older definitions remain unchanged.
- Each affected versioned runtime API function has new `Vn+1` input and output payloads, and its versioned input and
  output payload enums contain new `Vn+1` variants.
- The execution input and output payload types remain unversioned and have conversions to or from every supported wire
  payload version. Conversions for older versions continue preserving their existing wire shapes and semantics.
- Each affected existing versioned runtime API function handles the new variant and guarantees that `Vn+1` input
  produces `Vn+1` output.
- The entry for each updated runtime API function in `version_declarations()` advertises its new latest payload version.
- No new runtime API function, deprecation, cleanup, or ETH-RPC integration has been added as part of this procedure.
## Commands

<table>
  <thead>
    <tr>
      <th>Action</th>
      <th>Command</th>
      <th>Why</th>
    </tr>
  </thead>
  <tbody>
    <tr>
      <td>Check</td>
      <td>
        <code>SKIP_WASM_BUILD=1 SKIP_PALLET_REVIVE_FIXTURES=1 cargo clippy -p pallet-revive
        -p pallet-revive-eth-rpc -p revive-dev-runtime -p pallet-revive-types</code>
      </td>
      <td>
        The other packages added to this command are needed since some of the errors we get when versioning never appear
        in pallet-revive and might only appear in the consumer (e.g., the <code>pallet-revive-eth-rpc</code>) or in a
        runtime which implements the runtime API of pallet revive (e.g., <code>revive-dev-runtime</code>)
      </td>
    </tr>
    <tr>
      <td>Formatting</td>
      <td><code>cargo +nightly-2026-01-27 fmt --all</code></td>
      <td>LLMs get it wrong all the time</td>
    </tr>
  </tbody>
</table>

## Invariants

This section outlines various invariants and constraints which must be met when versioning a new runtime API function or
when updating an existing versioned runtime API function.

- All versioned runtime API functions have exactly one argument which is the versioned enum of all of its input versions
  and exactly one return type which is a versioned enum of all of its output versions. This applies even if we have a
  runtime API function which takes no arguments and returns nothing.
- Calling a runtime API function with a `Vn` input guarantees a `Vn` output on a successful call (or an error on
  fallible runtime API functions).
- The scale encoding of all of the V1 types must be identical to those used in the unversioned runtime API functions.
  E.g., `TraceV1` must be byte-by-byte identical to `Trace` such that encoding either allows us to decode as the other.
