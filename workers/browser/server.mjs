import { InputError, requireInput, publicUrl, describe, populate } from './forms.mjs';
import { captureTypes, captureViewport, dismissCookieDialog, encodeRecording, prepareCookieDialog, recordingOptions, recordPage, screenshot } from './capture.mjs';
import { chmod, mkdtemp, rm, unlink } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { chromium, errors } from 'playwright';

const LIMIT = 96_000;
const socketPath = '/browser/browser.sock';
const readResources = new Set(['document', 'script', 'stylesheet', 'fetch', 'xhr']);
const visualResources = new Set(['image', 'font', 'media']);

export function allowsRead(method, resource, render, capture = false) {
    return (method === 'GET' || method === 'HEAD') &&
        (resource === 'document' || (render && readResources.has(resource)) ||
            (capture && visualResources.has(resource)));
}

export function readableUrl(value) {
    requireInput(typeof value === 'string' && value.length <= 2048, 'Expected a public HTTPS URL.');
    const fragmentStart = value.indexOf('#');
    if (fragmentStart === -1) return publicUrl(value);
    const url = publicUrl(value.slice(0, fragmentStart));
    url.hash = value.slice(fragmentStart);
    return url;
}

export async function visit(page, url, {waitUntil = 'domcontentloaded', validate = publicUrl} = {}) {
    let response;
    try {
        response = await page.goto(url.href, {waitUntil});
    } catch (error) {
        if (error instanceof errors.TimeoutError) {
            throw new InputError('The website took too long to respond.');
        }
        if (/\bnet::ERR_(?:TUNNEL_CONNECTION_FAILED|CONNECTION_(?:REFUSED|RESET)|CERT_[A-Z_]+|SSL_PROTOCOL_ERROR)\b/.test(error.message)) {
            throw new InputError('The browser could not establish an HTTPS connection to the website.');
        }
        throw error;
    }
    requireInput(response, 'The website did not return an HTTPS response.');
    requireInput(response.headers()['cf-mitigated'] !== 'challenge',
        'Cloudflare requires verification before this website can be read.');
    requireInput(response.ok(), `The website returned HTTP ${response.status()}.`);
    return validate(page.url());
}

export function indexedUrl(raw, host) {
    const link = new URL(raw);
    let target = link.href;
    if (link.hostname === host && host === 'www.bing.com' && link.pathname === '/ck/a') {
        const encoded = link.searchParams.get('u');
        requireInput(encoded?.startsWith('a1'), 'Invalid Bing result link.');
        target = Buffer.from(encoded.slice(2), 'base64url').toString('utf8');
    } else if (link.hostname === host && host === 'www.google.com' && link.pathname === '/url') {
        target = link.searchParams.get('q') || link.searchParams.get('url');
    }
    const url = publicUrl(target);
    requireInput(url.hostname !== host, 'Expected a public result link.');
    return url.href;
}

function domainName(value) {
    if (!value.includes('.') || !/^[\p{L}\p{N}.-]+$/u.test(value)) return null;
    try {
        const domain = new URL('https://' + value).hostname.replace(/\.$/, '');
        const labels = domain.split('.');
        return labels.length > 1 && /[a-z]/.test(labels.at(-1)) &&
            labels.every(label => /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(label)) ? domain : null;
    } catch { return null; }
}

function withinDomain(host, domain) {
    return host === domain || Boolean(host?.endsWith('.' + domain));
}

export async function search(page, query) {
    requireInput(typeof query === 'string' && query.trim() && query.length <= 240, 'Invalid search query.');
    const siteOperators = [...query.matchAll(/(?:^|\s)site:([^\s]+)/gi)];
    const sites = siteOperators.map(([, value]) =>
        domainName(value.replace(/^https?:\/\//i, '').split('/')[0])).filter(Boolean);
    requireInput(!siteOperators.length || sites.length, 'Choose a valid domain for the site: search.');
    const literalDomain = sites.length ? null : domainName(query.trim());
    const domains = literalDomain ? [literalDomain] : sites;
    const mentions = new Map();
    const providers = [
        ['www.bing.com', 'li.b_algo h2 a'],
        ['www.google.com', 'a h3'],
    ];
    for (const [host, selector] of providers) {
        try {
            const url = new URL(`https://${host}/search`);
            url.searchParams.set('q', query);
            await visit(page, url);
            const candidates = await page.locator(selector).evaluateAll(headlines =>
                headlines.slice(0, 18).map(headline => ({
                    title: headline.textContent.trim().slice(0, 150),
                    url: headline.closest('a').href,
                    description: headline.closest('li.b_algo')?.querySelector('.b_caption p')
                        ?.textContent.trim().slice(0, 360) || '',
                })));
            const results = candidates.flatMap(candidate => {
                try { return [{...candidate, url: indexedUrl(candidate.url, host)}]; }
                catch { return []; }
            });
            const matching = results.filter(result => {
                const host = new URL(result.url).hostname.replace(/\.$/, '');
                return !domains.length || domains.some(domain => withinDomain(host, domain));
            });
            if (matching.length) return {results: matching.slice(0, 8)};
            if (literalDomain) {
                for (const result of results) {
                    const words = (result.title + ' ' + result.description).match(/[\p{L}\p{N}.-]+/gu) || [];
                    if (words.some(word => withinDomain(domainName(word), literalDomain))) {
                        mentions.set(result.url, result);
                    }
                }
            }
        } catch {
            // Search providers can redirect or challenge automated browsers.
        }
    }
    if (mentions.size) return {results: [...mentions.values()].slice(0, 8)};
    throw new InputError(domains.length ?
        'The search providers did not return a result for this domain.' :
        'The search providers did not return public results.');
}

export async function operate(browser, route, data, signal) {
    signal?.throwIfAborted();
    const recording = route === '/record' ? recordingOptions(data) : null;
    const capture = captureTypes.has(route) ? {
        url: readableUrl(data?.url), recording, dismissCookieDialog: dismissCookieDialog(data),
    } : null;
    const directory = recording ? await mkdtemp(join(tmpdir(), 'tinybot-recording-')) : null;
    try {
        const context = await browser.newContext({
            javaScriptEnabled: route === '/browse' || capture !== null,
            serviceWorkers: 'block', acceptDownloads: false,
            ...(capture ? {viewport: captureViewport, deviceScaleFactor: 1} : {}),
            ...(directory ? {recordVideo: {dir: directory, size: captureViewport}} : {}),
        });
        let closing;
        const closeContext = () => (closing ??= context.close());
        const closeOnAbort = () => void closeContext().catch(() => {});
        signal?.addEventListener('abort', closeOnAbort, {once: true});
        let result;
        try {
            signal?.throwIfAborted();
            context.setDefaultTimeout(8000);
            context.setDefaultNavigationTimeout(12000);
            result = await operatePage(context, route, data, capture, signal);
        } finally {
            signal?.removeEventListener('abort', closeOnAbort);
            await closeContext();
        }
        if (!directory) return result;
        return {...result, bytes: await encodeRecording(result.bytes, directory, recording, signal)};
    } finally {
        if (directory) await rm(directory, {recursive: true, force: true});
    }
}

async function operatePage(context, route, data, capture, signal) {
    const render = route === '/browse' || capture !== null;
    let permit;
    let posted = false;
    let postStatus;
    const page = await context.newPage();
    if (capture) context.on('page', popup => void popup.close().catch(() => {}));
    await context.routeWebSocket('**/*', socket => socket.close());
    await context.route('**/*', async intercepted => {
        const request = intercepted.request();
        let url;
        try { url = render ? readableUrl(request.url()) : publicUrl(request.url()); }
        catch { await intercepted.abort(); return; }
        if (permit) {
            if (!posted && request.method() === 'POST' && request.isNavigationRequest() && url.href === permit) {
                posted = true;
                try {
                    const response = await intercepted.fetch({maxRedirects: 0, timeout: 8000});
                    postStatus = response.status();
                    await response.dispose();
                    await intercepted.fulfill({status: 204});
                } catch { await intercepted.abort(); }
            } else await intercepted.abort();
            return;
        }
        if (allowsRead(request.method(), request.resourceType(), render, capture)) {
            await intercepted.continue();
        } else await intercepted.abort();
    });
    if (route === '/search') return await search(page, data.query);
    if (render) {
        await visit(page, capture?.url ?? readableUrl(data.url), {waitUntil: 'commit', validate: readableUrl});
        await page.waitForLoadState('domcontentloaded');
        if (capture) {
            await waitForPageLoad(page);
            const destination = readableUrl(page.url());
            const cookieDialog = await prepareCookieDialog(page, capture.dismissCookieDialog, signal);
            requireInput(page.url() === destination.href, 'The page navigated away while checking the cookie dialog.');
            const bytes = capture.recording ? await recordPage(page, capture.recording, signal) : await screenshot(page);
            readableUrl(page.url());
            return {bytes, cookieDialog};
        }
        // An already-rendered article can be read while unrelated resources are still loading.
        const readable = await page.locator('main, article, [role="main"]').evaluateAll(regions =>
            regions.slice(0, 8).some(region =>
                !region.closest('[aria-busy="true"]') &&
                !region.querySelector('[aria-busy="true"]') &&
                region.checkVisibility({opacityProperty: true, visibilityProperty: true}) &&
                region.innerText?.trim().length >= 200));
        if (!readable) await waitForPageLoad(page);
        const content = await page.locator('body').evaluate(body => ({
            title: document.title.slice(0, 200),
            text: body.innerText.slice(0, 6900),
            links: [...body.querySelectorAll('a[href]')].slice(0, 60)
                .filter(anchor => typeof anchor.href === 'string')
                .map(anchor => ({text: anchor.textContent.trim().slice(0, 90), url: anchor.href.slice(0, 500)}))
                .filter(anchor => anchor.text && anchor.url.startsWith('https://')).slice(0, 16),
        }));
        return {url: readableUrl(page.url()).href, ...content};
    }
    if (route === '/inspect') {
        const index = data.index ?? 0;
        requireInput(Number.isInteger(index) && index >= 0 && index <= 40, 'Choose a valid form position.');
        await visit(page, publicUrl(data.url));
        return describe(page, index);
    }
    requireInput(route === '/submit' && data.form, 'Invalid browser operation.');
    const confirmed = data.form;
    await visit(page, publicUrl(confirmed.page));
    const observed = await populate(page, confirmed, data.values);
    requireInput(page.url() === observed.page, 'The site navigated away from the confirmed form.');
    permit = observed.destination;
    const response = page.waitForResponse(r => r.request().method() === 'POST' && r.url() === permit);
    // Keep the internal promise observed even if a failed navigation detaches the evaluator.
    const submit = page.evaluate(index => HTMLFormElement.prototype.submit.call(document.forms[index]), observed.index);
    await Promise.all([response, submit.catch(() => undefined)]);
    requireInput(posted && postStatus >= 200 && postStatus < 400,
        'The site did not confirm the submission request.');
    return {submitted: true, http_status: postStatus};
}

async function waitForPageLoad(page) {
    try { await page.waitForLoadState('load', {timeout: 3000}); }
    catch (error) { if (!(error instanceof errors.TimeoutError)) throw error; }
}

async function readJson(request) {
    let length = 0;
    const chunks = [];
    for await (const chunk of request) {
        length += chunk.length;
        requireInput(length <= LIMIT, 'Browser request is too large.');
        chunks.push(chunk);
    }
    try { return JSON.parse(Buffer.concat(chunks).toString('utf8')); }
    catch { throw new InputError('Invalid browser request.'); }
}

async function serve() {
    const browser = await chromium.launch({
        chromiumSandbox: true,
        proxy: {server: 'http://egress:3128'},
        args: ['--proxy-bypass-list=<-loopback>', '--disable-quic', '--disable-background-networking']
    });
    browser.on('disconnected', () => process.exit(1));
    let active = 0;
    const server = createServer(async (request, response) => {
        const json = (status, data) => {
            response.writeHead(status, {'Content-Type': 'application/json', 'Cache-Control': 'no-store'});
            response.end(JSON.stringify(data));
        };
        if (request.method === 'GET' && request.url === '/health') {
            const release = process.env.TINYBOT_RELEASE_SHA;
            return json(200, {ok: true, release: /^[0-9a-f]{40}$/.test(release ?? '') ? release : null});
        }
        const contentType = captureTypes.get(request.url);
        if (request.method !== 'POST' || (!contentType && !['/browse', '/inspect', '/submit', '/search'].includes(request.url))) {
            return json(404, {error: 'Unknown browser operation.'});
        }
        if (active >= 4) return json(503, {error: 'Browser worker is busy.'});
        active += 1;
        const disconnected = new AbortController();
        response.once('close', () => disconnected.abort());
        try {
            const signal = contentType ? AbortSignal.any([disconnected.signal, AbortSignal.timeout(35000)]) : undefined;
            const result = await operate(browser, request.url, await readJson(request), signal);
            if (response.destroyed) return;
            if (contentType) {
                const headers = {'Content-Type': contentType, 'Cache-Control': 'no-store', 'Content-Length': result.bytes.length};
                if (result.cookieDialog) headers['X-TinyBot-Cookie-Dialog'] = result.cookieDialog;
                response.writeHead(200, headers);
                response.end(result.bytes);
            } else {
                json(200, result);
            }
        } catch (error) {
            // Playwright errors may include request metadata. Keep them inside the worker.
            if (!response.destroyed && !response.headersSent) {
                json(422, {error: error instanceof InputError ? error.message : 'Browser operation did not finish.'});
            }
        } finally {
            active -= 1;
        }
    });
    server.requestTimeout = 30000;
    await unlink(socketPath).catch(error => { if (error.code !== 'ENOENT') throw error; });
    server.listen(socketPath, () => void chmod(socketPath, 0o660).catch(() => process.exit(1)));
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
    serve().catch(error => {
        // Startup finishes before the worker accepts any user requests.
        console.error('TinyBot browser failed to start with Chromium isolation enabled.', error.message);
        process.exit(1);
    });
}
