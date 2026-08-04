// twofa.js — authenticated TOTP enrollment and one-time recovery-code UI.

import { api, ApiError } from './api.js';
import { toast } from './render.js';

const byId = (id) => document.getElementById(id);
const ui = {};
let forceReauth = () => {};
let currentSecret = '';
let currentUri = '';
let currentRecoveryCodes = [];

export function initTwoFactorUi(deps = {}) {
  if (typeof deps.forceReauth === 'function') forceReauth = deps.forceReauth;
  for (const id of [
    'twofa-status', 'btn-2fa-enroll', 'btn-2fa-cancel', 'twofa-setup',
    'twofa-secret', 'twofa-uri', 'btn-2fa-copy-uri', 'twofa-verify-code',
    'btn-2fa-verify', 'twofa-active', 'btn-2fa-recovery',
    'twofa-disable-code', 'btn-2fa-disable', 'twofa-recovery-result',
    'twofa-recovery-codes', 'btn-2fa-copy-codes',
  ]) ui[id] = byId(id);

  ui['btn-2fa-enroll'].addEventListener('click', enroll);
  ui['btn-2fa-cancel'].addEventListener('click', cancelPending);
  ui['btn-2fa-copy-uri'].addEventListener('click', () => copyText(currentUri, '配置 URI'));
  ui['btn-2fa-verify'].addEventListener('click', verify);
  ui['btn-2fa-recovery'].addEventListener('click', regenerateRecoveryCodes);
  ui['btn-2fa-disable'].addEventListener('click', disable);
  ui['btn-2fa-copy-codes'].addEventListener('click', () => {
    copyText(currentRecoveryCodes.join('\n'), '恢复码');
  });
}

export async function refreshTwoFactorStatus() {
  ui['twofa-status'].textContent = '正在读取安全设置…';
  try {
    renderStatus(await api.twoFactorStatus());
  } catch (error) {
    handleError(error, '读取两步验证状态失败');
  }
}

function renderStatus(status) {
  const activated = Boolean(status?.activated);
  const enrolled = Boolean(status?.enrolled);
  ui['twofa-status'].textContent = activated
    ? '已启用。登录需要验证器动态码或一次性恢复码。'
    : enrolled
      ? '设置尚未确认；可重新开始或取消待验证设置。'
      : '未启用。建议使用验证器应用保护账户。';
  ui['btn-2fa-enroll'].hidden = activated;
  ui['btn-2fa-enroll'].textContent = enrolled ? '重新开始设置' : '启用两步验证';
  ui['btn-2fa-cancel'].hidden = activated || !enrolled;
  ui['twofa-active'].hidden = !activated;
  if (activated) ui['twofa-setup'].hidden = true;
}

async function enroll() {
  await withBusy(ui['btn-2fa-enroll'], async () => {
    try {
      const result = await api.twoFactorEnroll();
      currentSecret = String(result?.secret || '');
      currentUri = String(result?.otpauth_uri || '');
      ui['twofa-secret'].textContent = currentSecret;
      ui['twofa-uri'].value = currentUri;
      ui['twofa-verify-code'].value = '';
      ui['twofa-setup'].hidden = false;
      ui['btn-2fa-cancel'].hidden = false;
      ui['twofa-status'].textContent = '把密钥添加到验证器，再输入当前 6 位动态码确认。';
    } catch (error) {
      handleError(error, '开始设置失败');
    }
  });
}

async function verify() {
  const code = ui['twofa-verify-code'].value.trim();
  if (!/^\d{6}$/.test(code)) {
    toast('请输入验证器中的 6 位动态码', 'error');
    return;
  }
  await withBusy(ui['btn-2fa-verify'], async () => {
    try {
      await api.twoFactorVerify(code);
      ui['twofa-setup'].hidden = true;
      renderStatus({ enrolled: true, activated: true });
      toast('两步验证已启用', 'ok');
      try {
        showRecoveryCodes(await api.twoFactorRecoveryCodes(code));
      } catch (error) {
        handleError(error, '已启用，但恢复码生成失败');
      }
    } catch (error) {
      handleError(error, '动态码验证失败');
    }
  });
}

async function regenerateRecoveryCodes() {
  const code = ui['twofa-disable-code'].value.trim();
  if (!/^\d{6}$/.test(code)) {
    toast('重置恢复码前请输入验证器中的 6 位动态码', 'error');
    return;
  }
  if (!confirm('生成新恢复码会立即作废旧恢复码，继续吗？')) return;
  await withBusy(ui['btn-2fa-recovery'], async () => {
    try {
      showRecoveryCodes(await api.twoFactorRecoveryCodes(code));
      ui['twofa-disable-code'].value = '';
      toast('新的恢复码已生成；旧恢复码已作废', 'ok');
    } catch (error) {
      handleError(error, '生成恢复码失败');
    }
  });
}

function showRecoveryCodes(result) {
  currentRecoveryCodes = Array.isArray(result?.codes) ? result.codes.map(String) : [];
  ui['twofa-recovery-codes'].textContent = currentRecoveryCodes.join('\n');
  ui['twofa-recovery-result'].hidden = currentRecoveryCodes.length === 0;
}

async function disable() {
  const code = ui['twofa-disable-code'].value.trim();
  if (!/^\d{6}$/.test(code)) {
    toast('关闭前请输入验证器中的 6 位动态码', 'error');
    return;
  }
  if (!confirm('关闭两步验证会同时作废全部恢复码，继续吗？')) return;
  await disableWithCode(code, ui['btn-2fa-disable']);
}

async function cancelPending() {
  await disableWithCode('', ui['btn-2fa-cancel']);
}

async function disableWithCode(code, button) {
  await withBusy(button, async () => {
    try {
      await api.twoFactorDisable(code);
      currentSecret = '';
      currentUri = '';
      currentRecoveryCodes = [];
      ui['twofa-setup'].hidden = true;
      ui['twofa-recovery-result'].hidden = true;
      ui['twofa-disable-code'].value = '';
      renderStatus({ enrolled: false, activated: false });
      toast(code ? '两步验证已关闭' : '待验证设置已取消', 'ok');
    } catch (error) {
      handleError(error, '关闭两步验证失败');
    }
  });
}

async function copyText(value, label) {
  if (!value) return;
  try {
    await navigator.clipboard.writeText(value);
    toast(`${label}已复制`, 'ok');
  } catch {
    toast(`无法复制${label}，请手动选择`, 'error');
  }
}

async function withBusy(button, task) {
  button.disabled = true;
  try {
    await task();
  } finally {
    button.disabled = false;
  }
}

function handleError(error, prefix) {
  if (error instanceof ApiError && error.status === 401) {
    forceReauth();
    return;
  }
  toast(`${prefix}:${error.message || error}`, 'error');
}
