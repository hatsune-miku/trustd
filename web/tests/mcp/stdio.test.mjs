import assert from 'node:assert/strict'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'

import { Client } from '@modelcontextprotocol/sdk/client/index.js'
import { StdioClientTransport, getDefaultEnvironment } from '@modelcontextprotocol/sdk/client/stdio.js'
import { ListToolsResultSchema } from '@modelcontextprotocol/sdk/types.js'

const command =
  process.env.TRUSTD_STDIO_BINARY ||
  fileURLToPath(
    new URL(`../../../target/debug/trustd-stdio${process.platform === 'win32' ? '.exe' : ''}`, import.meta.url)
  )
const identity = process.platform === 'win32' ? 'privilege_level' : 'sudo'
const properties = {
  secrets_list: ['query'],
  secrets_get: ['key'],
  enter_interactive: ['agent_name', 'request_reason', 'secret_keys', 'confirm_text'],
  run: ['command', 'stdin', 'timeout_seconds', identity],
  destroy: [],
}
const required = {
  secrets_list: ['query'],
  secrets_get: ['key'],
  enter_interactive: ['agent_name', 'request_reason', 'confirm_text'],
  run: ['command', identity],
  destroy: [],
}

test('official SDK lists a single authorization entry with no policy disclosures', async () => {
  // Listing tools needs no daemon, real token, authorization or shell execution.
  const transport = new StdioClientTransport({
    command,
    args: ['--endpoint', '127.0.0.1:1'],
    env: { ...getDefaultEnvironment(), TRUSTD_SCHEMA_TEST_TOKEN: 'fixture-not-a-real-token' },
    stderr: 'pipe',
  })
  const client = new Client({ name: 'trustd-schema-regression', version: '1' })
  transport.stderr?.resume()
  try {
    await client.connect(transport)
    const { tools } = await client.listTools()
    assert.doesNotThrow(() => ListToolsResultSchema.parse({ tools }))
    assert.deepEqual(
      tools.map((tool) => tool.name),
      ['secrets_list', 'secrets_get', 'enter_interactive', 'run', 'destroy']
    )
    for (const removed of ['preauthor', 'pre-author', 'remember', 'automatic', 'reused']) {
      assert.equal(JSON.stringify(tools).includes(removed), false)
    }
    for (const tool of tools) {
      assert.deepEqual(Object.keys(tool.inputSchema.properties).sort(), properties[tool.name].toSorted())
      assert.deepEqual((tool.inputSchema.required ?? []).toSorted(), required[tool.name].toSorted())
      assert.equal(tool.inputSchema.additionalProperties, false)
      for (const schema of [tool.inputSchema, tool.outputSchema]) {
        if (Object.hasOwn(schema, 'required')) {
          assert.ok(Array.isArray(schema.required), `${tool.name}: required must be an array, not null`)
          assert.ok(schema.required.length > 0, `${tool.name}: empty required must be omitted`)
          for (const key of schema.required) {
            assert.equal(typeof key, 'string')
            assert.ok(Object.hasOwn(schema.properties, key), `${tool.name}: required refers to an absent property`)
          }
        }
        assert.equal(Object.hasOwn(schema.properties, 'handle'), false)
        assert.equal(Object.hasOwn(schema.properties, 'token'), false)
      }
    }
    const querySchema = tools.find((tool) => tool.name === 'secrets_list').inputSchema.properties.query
    assert.equal(querySchema.minLength, 1)
    assert.equal(querySchema.pattern, '\\S')
    // Invalid searches fail locally, even with no daemon listening.
    for (const args of [{}, { query: '' }, { query: ' \t\n' }, { query: null }, { query: 123 }]) {
      const result = await client.callTool({ name: 'secrets_list', arguments: args })
      assert.equal(result.isError, true)
      assert.match(result.content[0].text, /query|missing required argument/)
      assert.equal(Object.hasOwn(result, 'structuredContent'), false)
    }
    // No open session: exercise an empty-argument tool and SDK output validation
    // without contacting a daemon or making an authorization decision.
    const destroyed = await client.callTool({ name: 'destroy', arguments: {} })
    assert.equal(destroyed.isError, false)
    assert.deepEqual(destroyed.structuredContent, { destroyed: true })
    // Guard the original failure against accidentally weakening the test/parser.
    const broken = structuredClone(tools)
    broken[0].inputSchema.required = null
    assert.throws(() => ListToolsResultSchema.parse({ tools: broken }))
  } finally {
    await client.close()
    await transport.close()
  }
})
