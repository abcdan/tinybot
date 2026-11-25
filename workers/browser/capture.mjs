import { execFile } from 'node:child_process';
import { readFile, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { promisify } from 'node:util';
import { InputError, requireInput } from './forms.mjs';

const encode = promisify(execFile);
const outputLimit = 5 * 1024 * 1024;
const rawRecordingLimit = 24 * 1024 * 1024;

export const captureTypes = new Map([['/screenshot', 'image/png'], ['/record', 'video/mp4']]);
export const captureViewport = {width: 960, height: 540};

export function recordingOptions(data) {
    const seconds = data?.duration_seconds ?? 5;
    const scroll = data?.scroll ?? false;
    requireInput(Number.isInteger(seconds) && seconds >= 2 && seconds <= 10,
        'Choose a recording duration from 2 to 10 seconds.');
    requireInput(typeof scroll === 'boolean', 'Expected a boolean scroll option.');
    return {seconds, scroll};
}

export async function screenshot(page) {
    const image = await page.screenshot({type: 'png', timeout: 3500});
    requireInput(image.length > 0 && image.length <= outputLimit, 'Screenshot is too large.');
    return image;
}

export async function recordPage(page, {seconds, scroll}, signal) {
    requireInput(page.video(), 'Browser recording is unavailable.');
    if (scroll) {
        const interval = 250;
        const deadline = performance.now() + seconds * 1000;
        while (performance.now() < deadline - interval) {
            await page.mouse.wheel(0, 100);
            await delay(interval, undefined, {signal});
        }
        await delay(Math.max(0, deadline - performance.now()), undefined, {signal});
    } else {
        await delay(seconds * 1000, undefined, {signal});
    }
    return page.video().path();
}

export async function encodeRecording(path, directory, {seconds}, signal) {
    const original = await stat(path);
    requireInput(original.size > 0 && original.size <= rawRecordingLimit, 'Recording is too large.');
    const target = join(directory, 'capture.mp4');
    try {
        await encode('ffmpeg', [
            '-nostdin', '-v', 'error', '-protocol_whitelist', 'file', '-sseof', `-${seconds}`, '-i', path,
            '-t', String(seconds), '-an', '-c:v', 'libx264', '-threads', '1', '-preset', 'ultrafast',
            '-crf', '25', '-maxrate', '2M', '-bufsize', '2M', '-pix_fmt', 'yuv420p', '-movflags', '+faststart', target,
        ], {timeout: 6000, killSignal: 'SIGKILL', maxBuffer: 8192, signal});
    } catch {
        throw new InputError('The browser recording could not be encoded.');
    }
    const result = await stat(target);
    requireInput(result.size > 0 && result.size <= outputLimit, 'Recording is too large.');
    return readFile(target, {signal});
}
