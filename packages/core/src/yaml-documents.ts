import { parseAllDocuments } from 'yaml'

/** Parse every document of a YAML file. Throws the message, and the YAML error as cause. */
export function parseYamlDocuments(contents: string): { documents: unknown[] } | { error: Error } {
  let documents
  try {
    documents = parseAllDocuments(contents, { merge: true })
  } catch (error) {
    return { error: new Error('Flow is not valid YAML', { cause: error }) }
  }
  const values: unknown[] = []
  for (const document of documents) {
    if (document.errors.length > 0) {
      return {
        error: new Error(`Flow is not valid YAML: ${document.errors[0].message}`, {
          cause: document.errors[0],
        }),
      }
    }
    values.push(document.toJS())
  }
  return { documents: values }
}
