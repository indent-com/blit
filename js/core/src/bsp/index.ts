export {
  parseDSL,
  serializeDSL,
  collectTags,
  leafCount,
  DSLParseError,
} from "./dsl";
export type { BSPNode, BSPSplit, BSPChild, BSPLeaf } from "./dsl";

export {
  PRESETS,
  enumeratePanes,
  assignSessionsToPanes,
  buildCandidateOrder,
  reconcileAssignments,
  adjustWeights,
  layoutFromDSL,
  surfaceAssignment,
  isSurfaceAssignment,
  parseSurfaceAssignment,
} from "./layout";
export type { BSPLayout, BSPPane, BSPAssignments } from "./layout";
