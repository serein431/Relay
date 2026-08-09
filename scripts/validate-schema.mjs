import { readFile } from "node:fs/promises";
import Ajv2020 from "ajv/dist/2020.js";
import addFormats from "ajv-formats";

const schemaUrl = new URL("../schemas/relay-handoff-v1.schema.json", import.meta.url);
const exampleUrl = new URL("../schemas/examples/minimal-relay-handoff-v1.json", import.meta.url);
const schema = JSON.parse(await readFile(schemaUrl, "utf8"));
const example = JSON.parse(await readFile(exampleUrl, "utf8"));

const ajv = new Ajv2020({ allErrors: true, strict: false });
addFormats(ajv);
const validate = ajv.compile(schema);

if (!validate(example)) {
  console.error(validate.errors);
  throw new Error("Relay Handoff example does not satisfy the schema");
}

const forbiddenClassification = structuredClone(example);
forbiddenClassification.conversation.records[0].blocks[0].classification = "private_reasoning";
if (validate(forbiddenClassification)) {
  throw new Error("Schema accepted private reasoning as shareable content");
}

const unsafePath = structuredClone(example);
unsafePath.project.logical_root = "repo://../secret";
if (validate(unsafePath)) {
  throw new Error("Schema accepted an unsafe repository path");
}

console.log("Relay Handoff schema and negative checks passed");
