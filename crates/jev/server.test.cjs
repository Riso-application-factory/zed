'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const { createServer, retryDelay, validateJson } = require('./server.cjs');
const call = (name, argumentsValue) => ({ jsonrpc: '2.0', id: 3, method: 'tools/call', params: { name, arguments: argumentsValue } });
const yes = { context: { tests: 'passed' }, question: 'Ready?', criteria: { true: 'Ready', false: 'Blocked' } };
const noul = { model: 'jev-1.14.0', answers: { decision: { type: 'noul', noul: 0.9 } }, usage: { input_tokens: 10, output_tokens: 2 } };
const environment = { JEV_API_KEY: 'test-secret' };
const parsed = response => JSON.parse(response.result.content[0].text);

test('initialize, notifications, ping, list and protocol errors', async () => {
  const handle = createServer({ environment: {} });
  assert.equal((await handle({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-03-26' } })).result.protocolVersion, '2025-03-26');
  assert.equal((await handle({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: 'future' } })).result.protocolVersion, '2024-11-05');
  assert.equal(await handle({ jsonrpc: '2.0', method: 'notifications/initialized' }), null);
  assert.deepEqual((await handle({ jsonrpc: '2.0', id: 1, method: 'ping' })).result, {});
  assert.equal((await handle({ jsonrpc: '2.0', id: 2, method: 'tools/list' })).result.tools.length, 3);
  assert.equal((await handle(null)).error.code, -32600);
  assert.equal((await handle({ jsonrpc: '2.0', id: 1, method: 'unknown' })).error.code, -32601);
});

test('missing key gives actionable tool error; inputs checked before HTTP', async () => {
  const handle = createServer({ environment: {}, fetch: () => assert.fail('unexpected HTTP') });
  const response = await handle(call('jev_yes_no', yes));
  assert.equal(response.result.isError, true);
  assert.match(response.result.content[0].text, /context_servers\.jev\.settings\.api_key.*JEV_API_KEY/);
  for (const message of [call('jev_yes_no', { ...yes, criteria: {} }), call('jev_score', { ...yes, criteria: [] }), call('jev_decide', { state: {}, questions: {} }), call('jev_decide', { state: {}, questions: { bad: { type: 'invented' } } }), call('jev_yes_no', { ...yes, unexpected: true })]) {
    assert.equal((await handle(message)).result.isError, true);
  }
});

test('separate key environment and nonsecret settings produce exact noul request', async () => {
  const handle = createServer({ environment: { JEV_API_KEY: 'settings-secret', JEV_SETTINGS: JSON.stringify({ endpoint: 'https://example.test/api', model: 'custom', timeout: 5 }) }, fetch: async (url, options) => {
    assert.equal(url, 'https://example.test/api');
    assert.equal(options.headers.Authorization, 'Bearer settings-secret');
    assert.equal(options.redirect, 'error');
    assert.deepEqual(JSON.parse(options.body), { model: 'custom', state: yes.context, questions: { decision: { type: 'noul', instructions: yes.question, criteria: yes.criteria } } });
    return Response.json(noul);
  } });
  const response = await handle(call('jev_yes_no', yes));
  assert.equal(response.result.isError, false);
  assert.deepEqual(parsed(response), { model: noul.model, probability: 0.9 });
});

test('server uses only JEV_API_KEY, never a key inside JEV_SETTINGS', async () => {
  const handle = createServer({ environment: { ...environment, JEV_SETTINGS: '{"api_key":"ignored-secret"}' }, fetch: async (_, options) => {
    assert.equal(options.headers.Authorization, 'Bearer test-secret');
    return Response.json(noul);
  } });
  assert.equal((await handle(call('jev_yes_no', yes))).result.isError, false);
});

test('score preserves fractional score, confidence, legend and probabilities', async () => {
  const answer = { type: 'score', score: 0.75, confidence: 0.5, legend: { 0: 'Low', 1: 'High' }, probabilities: { 0: 0.25, 1: 0.75 } };
  const handle = createServer({ environment, fetch: async () => Response.json({ model: noul.model, answers: { decision: answer } }) });
  assert.deepEqual(parsed(await handle(call('jev_score', { ...yes, criteria: ['Low', 'High'] }))), { model: noul.model, ...answer });
});

test('general mixed questions preserve raw response and enable image header', async () => {
  const body = { state: { photo: 'image-id' }, questions: {
    flag: { type: 'noul' }, route: { type: 'choice', instructions: { task: ['Route'] }, criteria: { accept: null, reject: { detail: 'Reject' } } },
    location: { type: 'box', instructions: 'Find photo' },
  }, assets: { 'image-id': { type: 'image', format: 'png', data_base64: 'aGVsbG8=' } } };
  const response = { model: 'vision-model', answers: {
    flag: { type: 'noul', noul: 0.1 }, route: { type: 'choice', choice: 'accept', confidence: 1, probabilities: { accept: 1, reject: 0 } },
    location: { type: 'box', box: { top: 0, bottom: 1, left: 0.2, right: 0.9 }, confidence: null },
  }, usage: { input_tokens: 30, output_tokens: 20 } };
  const handle = createServer({ environment, fetch: async (_, options) => {
    assert.equal(options.headers['TypeSafe-Danger-Zone-Images'], 'true');
    assert.deepEqual(JSON.parse(options.body), { ...body, model: noul.model });
    return Response.json(response);
  } });
  assert.deepEqual(parsed(await handle(call('jev_decide', body))), response);
});

test('429 and 529 retry with backoff and Retry-After', async () => {
  const delays = [];
  let attempts = 0;
  const handle = createServer({ environment, sleep: async delay => delays.push(delay), fetch: async () => {
    attempts++;
    return attempts < 3 ? new Response('', { status: attempts === 1 ? 429 : 529, headers: { 'Retry-After': '3' } }) : Response.json(noul);
  } });
  assert.equal((await handle(call('jev_yes_no', yes))).result.isError, false);
  assert.deepEqual(delays, [3000, 3000]);
  assert.equal(retryDelay('Wed, 21 Oct 2015 07:28:00 GMT', 0, Date.parse('Wed, 21 Oct 2015 07:27:55 GMT')), 5000);
  assert.equal(retryDelay('Wed, 21 Oct 2015 07:28:00 GMT', 0, Date.parse('Wed, 21 Oct 2015 07:28:05 GMT')), 1000);
  assert.throws(() => retryDelay('invalid', 0, 0), /Retry-After/);
});

test('retry limit, deadline and timeout become tool errors', async () => {
  let attempts = 0;
  const handle = createServer({ environment, sleep: async () => {}, fetch: async () => { attempts++; return new Response('', { status: 529 }); } });
  assert.match((await handle(call('jev_yes_no', yes))).result.content[0].text, /retry limit/);
  assert.equal(attempts, 4);
  const delayed = createServer({ environment, fetch: async () => new Response('', { status: 429, headers: { 'Retry-After': '100' } }) });
  assert.match((await delayed(call('jev_yes_no', yes))).result.content[0].text, /timeout/);
  const timeout = createServer({ environment: { ...environment, JEV_SETTINGS: '{"timeout":0.01}' }, fetch: (_, options) => new Promise((resolve, reject) => options.signal.addEventListener('abort', () => reject(new Error('abort')))) });
  assert.match((await timeout(call('jev_yes_no', yes))).result.content[0].text, /timed out/);
});

test('malformed API responses and network failures do not expose secrets', async () => {
  for (const response of [{}, { ...noul, answers: {} }, { ...noul, answers: { decision: { type: 'noul', noul: 2 } } }]) {
    const handle = createServer({ environment, fetch: async () => Response.json(response) });
    assert.equal((await handle(call('jev_yes_no', yes))).result.isError, true);
  }
  for (const fetch of [async () => { throw new Error('test-secret'); }, async () => new Response('test-secret', { status: 401 }), async () => new Response('test-secret')]) {
    const response = await createServer({ environment, fetch })(call('jev_yes_no', yes));
    assert.equal(response.result.isError, true);
    assert.doesNotMatch(JSON.stringify(response), /test-secret/);
  }
});

test('settings errors, question limits, nonfinite JSON and nesting limits', async () => {
  for (const settings of ['bad json', 'null', '{"timeout":0}', '{"endpoint":"file:///tmp/key"}', '{"model":4}']) {
    assert.equal((await createServer({ environment: { ...environment, JEV_SETTINGS: settings } })(call('jev_yes_no', yes))).result.isError, true);
  }
  const handle = createServer({ environment, fetch: () => assert.fail('unexpected HTTP') });
  assert.equal((await handle(call('jev_score', { ...yes, criteria: Array(11).fill('level') }))).result.isError, true);
  assert.equal((await handle(call('jev_decide', { state: {}, questions: { choice: { type: 'choice', criteria: Object.fromEntries(Array.from({ length: 256 }, (_, index) => [index, 'option'])) } } }))).result.isError, true);
  assert.throws(() => validateJson({ value: Infinity }), /finite/);
  let nested = {};
  for (let index = 0; index < 66; index++) nested = { nested };
  assert.throws(() => validateJson(nested), /nested/);
});

test('stdio recovers from malformed lines and continues after missing-key tool errors', () => {
  const messages = [
    '{bad json}',
    JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2024-11-05' } }),
    JSON.stringify({ jsonrpc: '2.0', id: 2, method: 'tools/list' }),
    JSON.stringify(call('jev_yes_no', yes)),
  ];
  const child = spawnSync(process.execPath, [require.resolve('./server.cjs')], { input: messages.join('\n') + '\n', encoding: 'utf8', env: { ...process.env, JEV_API_KEY: '', JEV_SETTINGS: '{}' } });
  assert.equal(child.status, 0);
  assert.equal(child.stderr, '');
  const responses = child.stdout.trim().split('\n').map(line => JSON.parse(line));
  assert.equal(responses.length, 4);
  assert.equal(responses[0].error.code, -32700);
  assert.equal(responses.find(response => response.id === 3).result.isError, true);
});

test('64 container levels are accepted, 65 and oversized bodies are rejected', async () => {
  let nested = {};
  for (let index = 1; index < 64; index++) nested = { nested };
  validateJson(nested);
  assert.throws(() => validateJson({ nested }), /nested/);
  const handle = createServer({ environment, fetch: () => assert.fail('unexpected HTTP') });
  const response = await handle(call('jev_decide', { state: { data: 'x'.repeat(32 * 1024 * 1024) }, questions: { check: { type: 'noul' } } }));
  assert.match(response.result.content[0].text, /32 MiB/);
});


test('validation errors expose bounded, redacted detail or code only for 400/413/422', async () => {
  for (const status of [400, 413, 422]) {
    for (const body of [{ detail: 'max_tokens_exceeded: test-secret ' + 'x'.repeat(1000) }, { code: 'max_tokens_exceeded' }, { error: { code: 'invalid_question' } }, { detail: [{ loc: ['body', 'questions'], msg: 'bad field test-secret' }] }]) {
      const response = await createServer({ environment, fetch: async () => Response.json(body, { status }) })(call('jev_yes_no', yes));
      const text = response.result.content[0].text;
      assert.equal(response.result.isError, true);
      assert.match(text, new RegExp(`Jev HTTP ${status}:`));
      assert.doesNotMatch(text, /test-secret/);
      assert.ok(text.length <= 514);
      assert.match(text, /max_tokens_exceeded|invalid_question|bad field/);
    }
    const response = await createServer({ environment, fetch: async () => new Response('not JSON test-secret', { status }) })(call('jev_yes_no', yes));
    assert.match(response.result.content[0].text, /request failed/);
    assert.doesNotMatch(response.result.content[0].text, /test-secret/);
  }
  const response = await createServer({ environment, fetch: async () => Response.json({ detail: 'private-detail' }, { status: 500 }) })(call('jev_yes_no', yes));
  assert.doesNotMatch(response.result.content[0].text, /private-detail/);
});

test('deep state, instructions and criteria fail before JSON serialization', async () => {
  let nested = {};
  for (let index = 0; index < 20000; index++) nested = { nested };
  const handle = createServer({ environment, fetch: () => assert.fail('unexpected HTTP') });
  for (const argumentsValue of [
    { state: nested, questions: { check: { type: 'noul' } } },
    { state: {}, questions: { check: { type: 'noul', instructions: nested } } },
    { state: {}, questions: { check: { type: 'score', criteria: [nested] } } },
  ]) {
    const response = await handle(call('jev_decide', argumentsValue));
    assert.match(response.result.content[0].text, /64 nested levels/);
    assert.doesNotMatch(response.result.content[0].text, /call stack/);
  }
});

test('short keys do not garble fixed errors', async () => {
  const response = await createServer({ environment: { JEV_API_KEY: 'k' }, fetch: async () => { throw new Error('network'); } })(call('jev_yes_no', yes));
  assert.equal(response.result.content[0].text, 'Jev connection failed; check endpoint and network');
});

test('JSON-RPC responses are ignored, invalid ids use null, unknown tools are invalid params', async () => {
  const handle = createServer({ environment: {} });
  for (const message of [{ jsonrpc: '2.0', id: 9, result: {} }, { jsonrpc: '2.0', id: 9, error: { code: -1 } }]) assert.equal(await handle(message), null);
  for (const id of [{ nested: 1 }, [], true, Infinity]) {
    const response = await handle({ jsonrpc: '2.0', id, method: 'ping' });
    assert.equal(response.id, null);
    assert.equal(response.error.code, -32600);
  }
  const response = await handle(call('unknown', {}));
  assert.equal(response.error.code, -32602);
  assert.equal(response.result, undefined);
});

test('all tools accept string, array and object context while rejecting scalar context', async () => {
  for (const state of ['Checks passed', ['Checks passed'], { tests: 'passed' }]) {
    const handle = createServer({ environment, fetch: async (_, options) => {
      const body = JSON.parse(options.body);
      assert.deepEqual(body.state, state);
      const type = body.questions.decision.type;
      return Response.json(type === 'noul' ? noul : { model: noul.model, answers: { decision: { type: 'score', score: 0, confidence: 1, legend: { 0: 'Ready' }, probabilities: { 0: 1 } } } });
    } });
    for (const message of [call('jev_yes_no', { ...yes, context: state }), call('jev_score', { ...yes, context: state, criteria: ['Ready'] }), call('jev_decide', { state, questions: { decision: { type: 'noul' } } })]) assert.equal((await handle(message)).result.isError, false);
  }
  for (const state of [null, 42, true]) assert.equal((await createServer({ environment })(call('jev_decide', { state, questions: { check: { type: 'noul' } } }))).result.isError, true);
});

test('attempted failures emit sanitized events; local validation and successes do not', async () => {
  const events = [];
  const handle = createServer({ environment, reportFailure: event => events.push(event), fetch: async () => Response.json({ detail: 'test-secret PRIVATE_STATE' }, { status: 401 }) });
  await handle(call('jev_yes_no', { ...yes, context: 'PRIVATE_STATE' }));
  assert.equal(events.length, 1);
  assert.equal(events[0].status, 401);
  assert.equal(events[0].tool, 'jev_yes_no');
  assert.doesNotMatch(JSON.stringify(events), /test-secret|PRIVATE_STATE/);
  await handle(call('jev_yes_no', {}));
  assert.equal(events.length, 1);
  const success = createServer({ environment, reportFailure: event => events.push(event), fetch: async () => Response.json(noul) });
  await success(call('jev_yes_no', yes));
  assert.equal(events.length, 1);
  for (const fetch of [async () => { throw new Error('test-secret PRIVATE_STATE'); }, async () => Response.json({})]) {
    await createServer({ environment, reportFailure: event => events.push(event), fetch })(call('jev_yes_no', yes));
  }
  assert.deepEqual(events.map(event => event.kind), ['http_401', 'network', 'invalid_response']);
  assert.doesNotMatch(JSON.stringify(events), /test-secret|PRIVATE_STATE/);
});

test('event file is capped and write failures never reject tool calls', async () => {
  const fs = require('node:fs/promises');
  const os = require('node:os');
  const path = require('node:path');
  const { writeFailureEvent, MAX_EVENTS_BYTES } = require('./server.cjs');
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'jev-events-'));
  try {
    const eventsPath = path.join(directory, 'events.jsonl');
    await fs.writeFile(eventsPath, 'x'.repeat(MAX_EVENTS_BYTES));
    const event = { timestamp: Date.now(), tool: 'jev_yes_no', kind: 'network', message: 'Network failed' };
    await writeFailureEvent(eventsPath, event);
    assert.ok((await fs.stat(eventsPath)).size <= MAX_EVENTS_BYTES);
    assert.deepEqual(JSON.parse((await fs.readFile(eventsPath, 'utf8')).trim()), event);
    const lines = [];
    const originalWrite = process.stderr.write;
    process.stderr.write = text => { lines.push(String(text)); return true; };
    try {
      const handle = createServer({ environment: { ...environment, JEV_EVENTS_PATH: path.join(directory, 'missing', 'events.jsonl') }, fetch: async () => { throw new Error('network'); } });
      assert.equal((await handle(call('jev_yes_no', yes))).result.isError, true);
      assert.equal((await handle(call('jev_yes_no', yes))).result.isError, true);
      await new Promise(resolve => setTimeout(resolve, 50));
      assert.equal(lines.length, 1);
      assert.doesNotMatch(lines.join(''), /test-secret/);
    } finally { process.stderr.write = originalWrite; }
  } finally { await fs.rm(directory, { recursive: true, force: true }); }
});

test('every non-2xx status and timeout is eligible for notification', async () => {
  const events = [];
  for (const status of [400, 401, 403, 413, 422, 429, 500, 529]) {
    await createServer({ environment, sleep: async () => {}, reportFailure: event => events.push(event), fetch: async () => new Response('', { status }) })(call('jev_yes_no', yes));
  }
  assert.deepEqual(events.map(event => event.status), [400, 401, 403, 413, 422, 429, 500, 529]);
  await createServer({ environment: { ...environment, JEV_SETTINGS: '{"timeout":0.01}' }, reportFailure: event => events.push(event), fetch: (_, options) => new Promise((resolve, reject) => options.signal.addEventListener('abort', () => reject(new Error('abort')))) })(call('jev_yes_no', yes));
  assert.equal(events.at(-1).kind, 'timeout');
});


test('Jev identity and keyless setup are exposed without failure events', async () => {
  const events = [];
  const handle = createServer({ environment: {}, reportFailure: event => events.push(event) });
  const initialized = await handle({jsonrpc:'2.0',id:1,method:'initialize',params:{protocolVersion:'2025-11-25'}});
  assert.match(initialized.result.instructions, /Jev.*TypeSafe System One/);
  const listed = await handle({jsonrpc:'2.0',id:2,method:'tools/list'});
  for (const tool of listed.result.tools) assert.ok(tool.description.startsWith('Jev (TypeSafe System One) decision support:'));
  const response = await handle(call('jev_yes_no', yes));
  assert.match(response.result.content[0].text, /Settings -> MCP Servers -> jev -> Configure/);
  assert.equal(events.length, 0);
});

test('jev_decide rejects a single question passed as questions with an actionable error, without a request', async () => {
  let fetched = false;
  const handle = createServer({ environment, fetch: async () => { fetched = true; throw new Error('unexpected request'); } });
  for (const type of ['noul', 'score', 'choice', 'box']) {
    const response = await handle(call('jev_decide', { state: 'evidence', questions: { type, instructions: 'Is it safe?', criteria: { true: 'Yes', false: 'No' } } }));
    assert.equal(response.result.isError, true);
    const text = response.result.content[0].text;
    assert.match(text, /^arguments\.questions must be an object keyed by question id, e\.g\. \{"q1": \{"type": "/);
    assert.match(text, new RegExp(`"type": "${type}"`));
    assert.match(text, /you passed a single question at the top level/);
  }
  // A question id that happens to be "type" is still a valid map entry, not a single question.
  const keyed = await handle(call('jev_decide', { state: 'evidence', questions: { type: { type: 'unknown' } } }));
  assert.doesNotMatch(keyed.result.content[0].text, /single question/);
  assert.equal(fetched, false);
});

test('tool descriptions and schemas explain argument shapes with examples', async () => {
  const handle = createServer({ environment: {} });
  const listed = await handle({ jsonrpc: '2.0', id: 2, method: 'tools/list' });
  const byName = Object.fromEntries(listed.result.tools.map(tool => [tool.name, tool]));
  const decide = byName.jev_decide;
  assert.match(decide.description, /`questions` is an object \(a map\) whose keys are YOUR OWN question ids \(for example "q1" or "is_safe"\)/);
  assert.match(decide.description, /never a single question object/);
  assert.match(decide.description, /Answers come back under `answers` with the same ids/);
  const example = /Example: (\{.*\}\}\}\})\./.exec(decide.description)?.[1];
  assert.ok(example, 'jev_decide description must contain an inline example');
  const parsedExample = JSON.parse(example);
  assert.equal(parsedExample.questions.q1.type, 'noul');
  const answered = await createServer({ environment, fetch: async () => new Response(JSON.stringify({ model: 'jev-1.14.0', answers: { q1: { type: 'noul', noul: 0.8 } } }), { status: 200 }) })(call('jev_decide', parsedExample));
  assert.equal(answered.result.isError, false);
  assert.equal(parsed(answered).answers.q1.noul, 0.8);
  const properties = decide.inputSchema.properties;
  assert.match(properties.state.description, /evidence/);
  assert.match(properties.questions.description, /Map from your own question ids to question objects/);
  assert.match(properties.questions.description, /Do not pass a single question object here/);
  assert.deepEqual(properties.questions.additionalProperties.oneOf.map(variant => /^(\w+)/.exec(variant.description)[1]), ['Noul', 'Score', 'Choice', 'Box']);
  for (const name of ['jev_yes_no', 'jev_score']) {
    const tool = byName[name];
    const toolExample = /Example: (\{.*?\]?\}?\})\. Returns/.exec(tool.description)?.[1];
    assert.ok(toolExample, `${name} description must contain an inline example`);
    const args = JSON.parse(toolExample);
    assert.deepEqual(Object.keys(args).sort(), ['context', 'criteria', 'question']);
    const result = await createServer({ environment, fetch: async () => new Response(JSON.stringify(name === 'jev_yes_no' ? noul : { model: 'jev-1.14.0', answers: { decision: { type: 'score', score: 1.2, confidence: 0.5, legend: {}, probabilities: { 0: 0.2, 1: 0.4, 2: 0.4 } } } }), { status: 200 }) })(call(name, args));
    assert.equal(result.result.isError, false, `${name} example must be accepted`);
    for (const property of ['context', 'question', 'criteria']) assert.ok(tool.inputSchema.properties[property].description, `${name}.${property} needs a description`);
  }
  assert.match(byName.jev_score.description, /criteria\[0\] is score 0/);
  assert.match(byName.jev_yes_no.description, /both `true` and `false` strings/);
});
