'use strict';
const readline = require('node:readline');
const fileSystem = require('node:fs/promises');
const { setTimeout: sleep } = require('node:timers/promises');

const MAX_BODY = 32 * 1024 * 1024;
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const content = { type: ['string', 'object', 'array', 'null'] };
const description = { type: ['string', 'object', 'array'] };
const described = (schema, text) => ({ ...schema, description: text });
const QUESTION_TYPES = ['noul', 'score', 'choice', 'box'];
const question = described({
  oneOf: [
    described({ type: 'object', required: ['type'], additionalProperties: false, properties: {
      type: { const: 'noul' }, instructions: described(content, 'The yes/no question to answer.'),
      criteria: described({ type: ['object', 'null'], additionalProperties: false, properties: { true: content, false: content } },
        'Optional: what counts as true and what counts as false.'),
    } }, 'Noul (yes/no): answer has probability `noul` of true (0–1). Example: {"type": "noul", "instructions": "Is this safe to deploy?", "criteria": {"true": "Backward compatible", "false": "Breaks existing queries"}}'),
    described({ type: 'object', required: ['type', 'criteria'], additionalProperties: false, properties: {
      type: { const: 'score' }, instructions: described(content, 'What to grade.'),
      criteria: described({ type: 'array', minItems: 1, maxItems: 10, items: description },
        '1–10 rubric levels from lowest to highest; the array index is the score (criteria[0] is score 0).'),
    } }, 'Score: answer has a fractional `score` (0 to criteria.length − 1), `confidence` and per-level `probabilities`. Example: {"type": "score", "instructions": "Rate deployment risk", "criteria": ["Low", "Moderate", "High"]}'),
    described({ type: 'object', required: ['type', 'criteria'], additionalProperties: false, properties: {
      type: { const: 'choice' }, instructions: described(content, 'What to choose.'),
      criteria: described({ type: 'object', minProperties: 1, maxProperties: 255, additionalProperties: content },
        'Options keyed by your own option ids, each mapped to its description (1–255 options).'),
    } }, 'Choice: answer has `choice` (one of your option ids), `confidence` and `probabilities`. Example: {"type": "choice", "instructions": "Who should review?", "criteria": {"database": "Database changes", "application": "Application changes"}}'),
    described({ type: 'object', required: ['type'], additionalProperties: false, properties: {
      type: { const: 'box' }, instructions: described(content, 'What to locate; reference an asset id from `assets`.'),
    } }, 'Box: locate something in an image from `assets`; answer has normalized `box` edges and nullable `confidence`. Example: {"type": "box", "instructions": "Locate the mug in photo-1"}'),
  ],
}, 'One question object: noul, score, choice or box.');
const asset = described({ type: 'object', required: ['type', 'format', 'data_base64'], additionalProperties: false,
  properties: { type: { const: 'image' }, format: { enum: ['png', 'jpg', 'jpeg'] }, data_base64: { type: 'string', minLength: 1 } } },
  'An inline image: {"type": "image", "format": "png", "data_base64": "..."}.');
const contextSchema = described(description, 'The evidence to decide on: a string, object or array. Include everything relevant; Jev sees nothing else.');
const tools = [
  { name: 'jev_decide', description: 'Jev (TypeSafe System One) decision support: Ask one or more independent, narrow questions about the same evidence in one call. Pass the evidence as `state`. `questions` is an object (a map) whose keys are YOUR OWN question ids (for example "q1" or "is_safe") and whose values are question objects; it is never a single question object or an array. Answers come back under `answers` with the same ids, plus `model` and `usage`. Question types: noul (yes/no probability), score (graded rubric), choice (pick one option id), box (locate an object in an image from `assets`). Example: {"state": {"change": "Adds a database index", "tests": "passed"}, "questions": {"q1": {"type": "noul", "instructions": "Is this safe to deploy?", "criteria": {"true": "Backward compatible", "false": "Breaks existing queries"}}}}. For a single yes/no or a single graded factor, jev_yes_no or jev_score is simpler. Treat uncertain answers as reasons for review.',
    inputSchema: { type: 'object', required: ['state', 'questions'], additionalProperties: false, properties: {
      state: described(description, 'The evidence all questions are judged against: a string, object or array. Include everything relevant; Jev sees nothing else.'),
      questions: described({ type: 'object', minProperties: 1, additionalProperties: question },
        'Map from your own question ids to question objects, e.g. {"q1": {"type": "noul", "instructions": "...", "criteria": {"true": "...", "false": "..."}}}. Each key is an id you choose (any string); each value must be one question object with a `type`. Do not pass a single question object here; wrap it under an id.'),
      assets: described({ type: 'object', minProperties: 1, additionalProperties: asset },
        'Optional: images keyed by your own asset ids, referenced by id from state or instructions (needed for box questions and an image-capable model).'),
    } } },
  { name: 'jev_yes_no', description: 'Jev (TypeSafe System One) decision support: Ask one narrow yes/no question (go/no-go or a risk check) about supplied evidence. Arguments: `context` (the evidence: string, object or array), `question` (a string), and `criteria` with both `true` and `false` strings describing what each answer means. Example: {"context": "Migration adds a nullable column", "question": "Is this safe to deploy?", "criteria": {"true": "Backward compatible", "false": "Breaks existing clients"}}. Returns `model` and `probability` that the answer is true (0–1); near 0.5 means uncertain. Use jev_decide to ask several questions at once.',
    inputSchema: { type: 'object', required: ['context', 'question', 'criteria'], additionalProperties: false, properties: {
      context: contextSchema, question: described({ type: 'string', minLength: 1 }, 'One narrow yes/no question.'),
      criteria: described({ type: 'object', required: ['true', 'false'], additionalProperties: false,
        properties: { true: described({ type: 'string', minLength: 1 }, 'What makes the answer true.'), false: described({ type: 'string', minLength: 1 }, 'What makes the answer false.') } },
        'Both outcomes, e.g. {"true": "Backward compatible", "false": "Breaks existing clients"}.'),
    } } },
  { name: 'jev_score', description: 'Jev (TypeSafe System One) decision support: Grade one factor such as risk or readiness against a rubric. Arguments: `context` (the evidence: string, object or array), `question` (what to grade), and `criteria`, an array of 1–10 level descriptions ordered from lowest to highest; the array index is the score, so criteria[0] is score 0. Example: {"context": "Patch touches auth and billing", "question": "How risky is this change?", "criteria": ["Low risk", "Moderate risk", "High risk"]}. Returns `model`, a fractional `score` (0 to criteria.length − 1), `confidence` (0–1), `legend` and per-level `probabilities`; use low confidence to request review.',
    inputSchema: { type: 'object', required: ['context', 'question', 'criteria'], additionalProperties: false, properties: {
      context: contextSchema, question: described({ type: 'string', minLength: 1 }, 'What to grade.'),
      criteria: described({ type: 'array', minItems: 1, maxItems: 10, items: { type: 'string', minLength: 1 } },
        '1–10 rubric levels from lowest to highest, e.g. ["Low risk", "Moderate risk", "High risk"]; index 0 is score 0.'),
    } } },
];

function ensure(condition, message) {
  if (!condition) throw new Error(message);
}

function validate(schema, value, path = 'arguments') {
  if (schema.oneOf) {
    ensure(schema.oneOf.some(candidate => { try { validate(candidate, value, path); return true; } catch { return false; } }), `${path}: invalid question; use noul, score (1–10 levels), choice (1–255 options), or box`);
    return;
  }
  if ('const' in schema) ensure(value === schema.const, `${path}: unexpected value`);
  if (schema.enum) ensure(schema.enum.includes(value), `${path}: unsupported value`);
  if (schema.type) {
    const actual = value === null ? 'null' : Array.isArray(value) ? 'array' : typeof value;
    ensure([].concat(schema.type).includes(actual), `${path}: expected ${schema.type}`);
  }
  if (typeof value === 'string' && schema.minLength) ensure(value.trim().length >= schema.minLength, `${path}: must not be empty`);
  if (Array.isArray(value)) {
    if (schema.minItems) ensure(value.length >= schema.minItems, `${path}: too few items`);
    if (schema.maxItems) ensure(value.length <= schema.maxItems, `${path}: too many items`);
    if (schema.items) value.forEach((item, index) => validate(schema.items, item, `${path}[${index}]`));
  }
  if (object(value)) {
    for (const key of schema.required || []) ensure(Object.hasOwn(value, key), `${path}.${key}: required`);
    if (schema.minProperties) ensure(Object.keys(value).length >= schema.minProperties, `${path}: too few entries`);
    if (schema.maxProperties) ensure(Object.keys(value).length <= schema.maxProperties, `${path}: too many entries`);
    for (const [key, entry] of Object.entries(value)) {
      if (Object.hasOwn(schema.properties || {}, key)) validate(schema.properties[key], entry, `${path}.${key}`);
      else if (schema.additionalProperties === false) throw new Error(`${path}.${key}: unknown field`);
      else if (object(schema.additionalProperties)) validate(schema.additionalProperties, entry, `${path}.${key}`);
    }
  }
}

// A frequent agent mistake is passing one question object as `questions`; the generic
// per-entry error ("questions.type: invalid question") does not say how to fix it.
function rejectSingleQuestion(argumentsValue) {
  const questions = object(argumentsValue) ? argumentsValue.questions : undefined;
  ensure(!(object(questions) && QUESTION_TYPES.includes(questions.type)),
    `arguments.questions must be an object keyed by question id, e.g. {"q1": {"type": "${object(questions) ? questions.type : 'noul'}", ...}}; you passed a single question at the top level. Wrap it under an id of your choice; answers come back under the same id.`);
}

function validateJson(value, depth = 0) {
  if (typeof value === 'number') ensure(Number.isFinite(value), 'Request numbers must be finite');
  if (value && typeof value === 'object') {
    ensure(depth < 64, 'Request exceeds the API limit of 64 nested levels');
    for (const entry of Object.values(value)) validateJson(entry, depth + 1);
  }
}

function configuration(environment) {
  let settings;
  try { settings = JSON.parse(environment.JEV_SETTINGS || '{}'); }
  catch { throw new Error('Invalid Jev settings JSON'); }
  ensure(object(settings), 'Jev settings must be an object');
  for (const name of ['model', 'endpoint']) {
    ensure(settings[name] === undefined || typeof settings[name] === 'string', `Jev setting ${name} must be a string`);
  }
  const apiKey = environment.JEV_API_KEY?.trim();
  ensure(apiKey, 'Missing Jev API key. Set the API key in Settings -> MCP Servers -> jev -> Configure (context_servers.jev.settings.api_key), or JEV_API_KEY.');
  const model = settings.model ?? 'jev-1.14.0';
  ensure(model.trim(), 'Jev model must not be empty');
  const endpoint = settings.endpoint ?? 'https://api.typesafe.ai/v1/systemone';
  let url;
  try { url = new URL(endpoint); } catch { throw new Error('Jev endpoint must be a valid HTTP(S) URL'); }
  ensure(['https:', 'http:'].includes(url.protocol) && !url.username && !url.password, 'Jev endpoint must be an HTTP(S) URL without credentials');
  const timeout = settings.timeout ?? 60;
  ensure(typeof timeout === 'number' && Number.isFinite(timeout) && timeout > 0 && timeout <= 3600, 'Jev timeout must be between 0 and 3600 seconds (exclusive of zero)');
  return { apiKey, model, endpoint, timeout };
}

function retryDelay(header, attempt, now) {
  const backoff = 1000 * 2 ** attempt;
  if (header === null || header === undefined) return backoff;
  const seconds = Number(header);
  const delay = header.trim() !== '' && Number.isFinite(seconds) ? seconds * 1000 : Math.max(0, Date.parse(header) - now);
  ensure(Number.isFinite(delay) && delay >= 0, 'Jev returned an invalid Retry-After header');
  return Math.max(backoff, delay);
}

function validateAnswers(response, questions) {
  ensure(object(response) && typeof response.model === 'string' && response.model.trim() && object(response.answers), 'Invalid Jev response: missing model or answers');
  const number = (value, maximum = 1) => typeof value === 'number' && Number.isFinite(value) && value >= 0 && value <= maximum;
  for (const [name, question] of Object.entries(questions)) {
    const answer = Object.hasOwn(response.answers, name) ? response.answers[name] : null;
    ensure(object(answer) && answer.type === question.type, `Invalid Jev response: answer type for ${name}`);
    if (question.type === 'noul') ensure(number(answer.noul), `Invalid Jev response: noul for ${name}`);
    else {
      ensure(number(answer.confidence) || (question.type === 'box' && answer.confidence === null), `Invalid Jev response: confidence for ${name}`);
      if (question.type === 'box') {
        ensure(object(answer.box) && ['top', 'bottom', 'left', 'right'].every(edge => number(answer.box[edge])), `Invalid Jev response: box for ${name}`);
      } else {
        const keys = question.type === 'score' ? question.criteria.map((_, index) => String(index)) : Object.keys(question.criteria);
        ensure(object(answer.probabilities) && keys.every(key => Object.hasOwn(answer.probabilities, key) && number(answer.probabilities[key])), `Invalid Jev response: probabilities for ${name}`);
        if (question.type === 'score') {
          ensure(number(answer.score, question.criteria.length - 1) && object(answer.legend), `Invalid Jev response: score or legend for ${name}`);
        } else ensure(typeof answer.choice === 'string' && Object.hasOwn(question.criteria, answer.choice), `Invalid Jev response: choice for ${name}`);
      }
    }
  }
}

const MAX_EVENTS_BYTES = 256 * 1024;
let eventWriteWarning = false;
async function writeFailureEvent(path, event) {
  let size = 0;
  try { size = (await fileSystem.stat(path)).size; }
  catch (error) { if (error.code !== 'ENOENT') throw error; }
  const line = JSON.stringify(event) + '\n';
  if (size + Buffer.byteLength(line) > MAX_EVENTS_BYTES) await fileSystem.writeFile(path, '');
  await fileSystem.appendFile(path, line, { mode: 0o600 });
}
function failureEvent(error, tool) {
  const status = Number(/^Jev HTTP (\d+)/.exec(error.message)?.[1]) || undefined;
  const kind = status ? `http_${status}` : /timeout|timed out|aborted/i.test(error.message) ? 'timeout' : /connection/i.test(error.message) ? 'network' : 'invalid_response';
  const message = status === 401 ? 'HTTP 401 API key rejected; check context_servers.jev.settings.api_key' : status ? `HTTP ${status}; check Jev settings or try again` : kind === 'network' ? 'Network failure; check connection and Jev endpoint' : kind === 'timeout' ? 'Request timed out; try again or increase settings.timeout' : 'Invalid Jev response; check model and endpoint';
  return { timestamp: Date.now(), tool, kind, ...(status ? { status } : {}), message };
}

async function requestApi(body, config, dependencies) {
  validateJson(body.state);
  for (const question of Object.values(body.questions)) {
    validateJson(question.instructions);
    validateJson(question.criteria);
  }
  const serialized = JSON.stringify({ ...body, model: config.model });
  ensure(Buffer.byteLength(serialized) <= MAX_BODY, 'Jev request exceeds the API body limit of 32 MiB');
  const controller = new AbortController();
  const deadline = dependencies.now() + config.timeout * 1000;
  const timer = setTimeout(() => controller.abort(), config.timeout * 1000);
  let attempted = false;
  try {
    for (let attempt = 0; attempt <= 3; attempt++) {
      let response;
      try {
        attempted = true;
        response = await dependencies.fetch(config.endpoint, {
          method: 'POST', redirect: 'error', signal: controller.signal,
          headers: { Authorization: `Bearer ${config.apiKey}`, 'Content-Type': 'application/json',
            ...(body.assets || Object.values(body.questions).some(question => question.type === 'box') ? { 'TypeSafe-Danger-Zone-Images': 'true' } : {}) },
          body: serialized,
        });
      } catch {
        throw new Error(controller.signal.aborted ? 'Jev request timed out' : 'Jev connection failed; check endpoint and network');
      }
      if ([429, 529].includes(response.status) && attempt < 3) {
        const delay = retryDelay(response.headers.get('retry-after'), attempt, dependencies.now());
        if (response.body) await response.body.cancel();
        ensure(dependencies.now() + delay < deadline, 'Jev retry exceeds timeout; try later or increase settings.timeout');
        await dependencies.sleep(delay, undefined, { signal: controller.signal });
        continue;
      }
      if (!response.ok) {
        let detail = '';
        if ([400, 413, 422].includes(response.status)) {
          try {
            const body = await response.json();
            const value = body?.detail ?? body?.code ?? body?.error?.code;
            if (value !== undefined) {
              const text = typeof value === 'string' ? value : JSON.stringify(value);
              detail = redactKey(text, config.apiKey).slice(0, 500);
            }
          } catch {
            detail = '';
          }
        } else if (response.body) await response.body.cancel();
        throw new Error(`Jev HTTP ${response.status}${detail ? `: ${detail}` : response.status === 401 ? ': API key rejected; check Jev settings' : response.status === 429 || response.status === 529 ? ': retry limit reached; try later' : ': request failed; check inputs, model and endpoint'}`);
      }
      let result;
      try { result = await response.json(); } catch { throw new Error(controller.signal.aborted ? 'Jev request timed out' : 'Jev returned invalid JSON'); }
      validateAnswers(result, body.questions);
      return result;
    }
  } catch (error) {
    if (attempted) dependencies.reportFailure(failureEvent(error, dependencies.tool));
    throw error;
  } finally { clearTimeout(timer); }
}

function redactKey(text, key) {
  return typeof key === 'string' && key.length >= 8 ? text.split(key).join('[redacted]') : text;
}

function responseId(message) {
  return typeof message?.id === 'string' || (typeof message?.id === 'number' && Number.isFinite(message.id)) ? message.id : null;
}

function createServer({ environment = process.env, fetch = globalThis.fetch, sleep: sleeper = sleep, now = Date.now, reportFailure } = {}) {
  reportFailure ??= event => {
    if (environment.JEV_EVENTS_PATH) writeFailureEvent(environment.JEV_EVENTS_PATH, event).catch(() => {
      if (!eventWriteWarning) { eventWriteWarning = true; process.stderr.write('Jev: could not record failure notification\n'); }
    });
  };
  const dependencies = { fetch, sleep: sleeper, now, reportFailure };
  const toolResult = (value, isError = false) => ({ content: [{ type: 'text', text: typeof value === 'string' ? value : JSON.stringify(value) }], isError });
  return async function handle(message) {
    const error = (code, text) => ({ jsonrpc: '2.0', id: responseId(message), error: { code, message: text } });
    if (object(message) && !Object.hasOwn(message, 'method')) return null;
    if (!object(message) || message.jsonrpc !== '2.0' || typeof message.method !== 'string' ||
        (Object.hasOwn(message, 'id') && !(typeof message.id === 'string' || (typeof message.id === 'number' && Number.isFinite(message.id)) || message.id === null))) return error(-32600, 'Invalid JSON-RPC request');
    if (!Object.hasOwn(message, 'id')) return null;
    let result;
    switch (message.method) {
      case 'initialize': {
        const requested = message.params?.protocolVersion;
        result = { protocolVersion: ['2024-11-05', '2025-03-26', '2025-06-18', '2025-11-25'].includes(requested) ? requested : '2024-11-05', capabilities: { tools: {} }, serverInfo: { name: 'jev', version: '0.1.0' }, instructions: 'Jev (TypeSafe System One) provides structured decision support. Use its tools for narrow go/no-go, risk, routing and graded decisions; supply concrete evidence and criteria. Configure the API key in Settings -> MCP Servers -> jev -> Configure.' };
        break;
      }
      case 'ping': result = {}; break;
      case 'tools/list': result = { tools }; break;
      case 'tools/call': {
        const tool = tools.find(tool => tool.name === message.params?.name);
        if (!tool) return error(-32602, 'Unknown Jev tool');
        try {
          const argumentsValue = message.params.arguments;
          if (tool.name === 'jev_decide') rejectSingleQuestion(argumentsValue);
          validate(tool.inputSchema, argumentsValue);
          let body;
          if (tool.name === 'jev_decide') body = argumentsValue;
          else body = { state: argumentsValue.context, questions: { decision: {
            type: tool.name === 'jev_yes_no' ? 'noul' : 'score', instructions: argumentsValue.question, criteria: argumentsValue.criteria,
          } } };
          const response = await requestApi(body, configuration(environment), { ...dependencies, tool: tool.name });
          result = toolResult(tool.name === 'jev_decide' ? response : {
            model: response.model, ...(tool.name === 'jev_yes_no' ? { probability: response.answers.decision.noul } : response.answers.decision),
          });
        } catch (failure) {
          const text = redactKey(failure instanceof Error ? failure.message : 'Jev request failed', environment.JEV_API_KEY?.trim());
          result = toolResult(text, true);
        }
        break;
      }
      default: return error(-32601, 'Method not found');
    }
    return { jsonrpc: '2.0', id: message.id, result };
  };
}

function startStdio() {
  const handle = createServer();
  const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  const write = message => { if (message !== null) process.stdout.write(`${JSON.stringify(message)}\n`); };
  input.on('line', line => {
    if (!line.trim()) return;
    if (Buffer.byteLength(line) > MAX_BODY + 1024 * 1024) {
      write({ jsonrpc: '2.0', id: null, error: { code: -32600, message: 'MCP message too large' } });
      return;
    }
    let message;
    try { message = JSON.parse(line); }
    catch { write({ jsonrpc: '2.0', id: null, error: { code: -32700, message: 'Parse error' } }); return; }
    handle(message).then(write).catch(() => write({ jsonrpc: '2.0', id: responseId(message), error: { code: -32603, message: 'Internal server error' } }));
  });
  process.stdout.on('error', () => { input.close(); process.exitCode = 1; });
}

module.exports = { writeFailureEvent, MAX_EVENTS_BYTES, createServer, retryDelay, validateJson, tools };
if (require.main === module) startStdio();
