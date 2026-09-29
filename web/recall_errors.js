// Compatibility bridge for the legacy migration-reference app. The shared
// production implementation lives under src/ so the Solid entry imports no
// root-level legacy modules.
export { recallErrorToast } from './src/recall_errors.js'
