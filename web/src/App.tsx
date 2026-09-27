import { createSignal, onMount, Show, type JSX } from 'solid-js'
import {
  IrisAlert,
  IrisAvatar,
  IrisBadge,
  IrisButton,
  IrisCard,
  IrisFormField,
  IrisIcon,
  IrisInput,
  IrisProvider,
  IrisSpinner,
  IrisTextarea,
  SkinProvider,
  createSkinEngine,
  darkSkin,
  lightSkin,
  localStorageSkinStorage,
} from '@iris-ui-kit/solid'
import { api, ApiError, sessionStorage, type Participant, type SessionResponse } from './api'
import { ChatShell } from './ChatShell'
import './style.css'

const skinEngine = createSkinEngine({
  skins: [lightSkin, darkSkin],
  default: 'dark',
  storage: localStorageSkinStorage('aero-im-solid-skin'),
})

type AuthMode = 'login' | 'register'

function AuthScreen(props: { onSuccess: (session: SessionResponse) => void }): JSX.Element {
  const [mode, setMode] = createSignal<AuthMode>('login')
  const [email, setEmail] = createSignal('')
  const [displayName, setDisplayName] = createSignal('')
  const [password, setPassword] = createSignal('')
  const [secondFactor, setSecondFactor] = createSignal('')
  const [busy, setBusy] = createSignal(false)
  const [error, setError] = createSignal('')

  const submit = async (event: Event): Promise<void> => {
    event.preventDefault()
    setBusy(true)
    setError('')
    try {
      const result = mode() === 'login'
        ? await api.login(email().trim(), password(), secondFactor().trim())
        : await api.register(email().trim(), displayName().trim(), password())
      props.onSuccess(result)
    } catch (reason) {
      setError(reason instanceof ApiError ? reason.message : '请求失败，请稍后重试')
    } finally {
      setBusy(false)
    }
  }

  return (
    <main class="auth-page">
      <div class="auth-card">
        <IrisCard padding="lg">
        <div class="brand-lockup">
          <div class="brand-mark">A</div>
          <div>
            <h1>Aero IM</h1>
            <p>AI-native messaging · SolidJS + Iris UI</p>
          </div>
        </div>

        <div class="auth-tabs" role="tablist" aria-label="认证方式">
          <button
            classList={{ active: mode() === 'login' }}
            type="button"
            onClick={() => setMode('login')}
          >
            登录
          </button>
          <button
            classList={{ active: mode() === 'register' }}
            type="button"
            onClick={() => setMode('register')}
          >
            注册
          </button>
        </div>

        <form class="auth-form" onSubmit={submit}>
          <IrisFormField label="邮箱">
            <IrisInput
              type="email"
              value={email()}
              required
              autocomplete="email"
              onInput={(event: InputEvent & { currentTarget: HTMLInputElement }) => setEmail(event.currentTarget.value)}
            />
          </IrisFormField>

          <Show when={mode() === 'register'}>
            <IrisFormField label="显示名">
              <IrisInput
                value={displayName()}
                required
                maxlength={64}
                autocomplete="nickname"
                onInput={(event: InputEvent & { currentTarget: HTMLInputElement }) => setDisplayName(event.currentTarget.value)}
              />
            </IrisFormField>
          </Show>

          <IrisFormField label="密码">
            <IrisInput
              type="password"
              value={password()}
              required
              minlength={6}
              autocomplete={mode() === 'login' ? 'current-password' : 'new-password'}
              onInput={(event: InputEvent & { currentTarget: HTMLInputElement }) => setPassword(event.currentTarget.value)}
            />
          </IrisFormField>

          <Show when={mode() === 'login'}>
            <IrisFormField label="两步验证码或恢复码">
              <IrisInput
                value={secondFactor()}
                autocomplete="one-time-code"
                placeholder="可选"
                onInput={(event: InputEvent & { currentTarget: HTMLInputElement }) => setSecondFactor(event.currentTarget.value)}
              />
            </IrisFormField>
          </Show>

          <Show when={error()}>
            <IrisAlert tone="danger">{error()}</IrisAlert>
          </Show>

          <IrisButton type="submit" variant="solid" disabled={busy()} style={{ width: '100%' }}>
            <Show when={!busy()} fallback={<IrisSpinner size="sm" />}>
              {mode() === 'login' ? '登录' : '注册并登录'}
            </Show>
          </IrisButton>
        </form>
        </IrisCard>
      </div>
    </main>
  )
}

function BootScreen(): JSX.Element {
  return (
    <main class="boot-page">
      <IrisSpinner size="lg" />
      <span>正在恢复会话…</span>
    </main>
  )
}

export function App(): JSX.Element {
  const [participant, setParticipant] = createSignal<Participant | null>(null)
  const [booting, setBooting] = createSignal(true)

  onMount(async () => {
    if (sessionStorage.token) {
      try {
        setParticipant(await api.me())
      } catch {
        sessionStorage.clear()
      }
    }
    setBooting(false)
  })

  const enter = (session: SessionResponse): void => {
    sessionStorage.set(session)
    setParticipant(session.participant)
  }

  const logout = async (): Promise<void> => {
    try {
      await api.logout()
    } catch {
      // The local session is still cleared even if the remote revoke is down.
    }
    sessionStorage.clear()
    setParticipant(null)
  }

  return (
    <SkinProvider engine={skinEngine}>
      <IrisProvider>
        <Show when={!booting()} fallback={<BootScreen />}>
          <Show when={participant()} fallback={<AuthScreen onSuccess={enter} />}>
            {(current) => <ChatShell participant={current()} onLogout={logout} />}
          </Show>
        </Show>
      </IrisProvider>
    </SkinProvider>
  )
}
