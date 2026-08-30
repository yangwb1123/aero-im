from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import unittest


PLATFORM_DIR = Path(__file__).resolve().parents[1]
COMPOSE_FILE = PLATFORM_DIR / "compose.yaml"


def load_bootstrap_module():
    path = PLATFORM_DIR / "scripts" / "bootstrap-audit.py"
    spec = importlib.util.spec_from_file_location("platform_audit_bootstrap", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load audit bootstrap module")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_snaplink_sync_module():
    path = PLATFORM_DIR / "scripts" / "sync-snaplink-config.py"
    spec = importlib.util.spec_from_file_location("platform_snaplink_sync", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load Snaplink config sync module")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class PlatformContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        completed = subprocess.run(
            [
                "docker",
                "compose",
                "-f",
                str(COMPOSE_FILE),
                "config",
                "--format",
                "json",
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        cls.compose = json.loads(completed.stdout)

    def test_required_platform_services_are_composed(self) -> None:
        expected = {
            "snaplink",
            "snaplink-config-sync",
            "snaplink-console",
            "audit-governance",
            "audit-bootstrap",
            "optional-api-disabled",
            "aero-id",
            "aero-vault",
            "aero-im",
            "aero-platform-console",
        }
        self.assertTrue(expected.issubset(self.compose["services"]))

    def test_public_ports_default_to_loopback(self) -> None:
        for service in self.compose["services"].values():
            for port in service.get("ports", []):
                self.assertEqual(port.get("host_ip"), "127.0.0.1")

    def test_sensitive_connectors_use_loopback_edges(self) -> None:
        expected_modes = {
            "audit-governance": "service:audit-governance-edge",
            "aero-id": "service:aero-id-edge",
            "aero-vault": "service:aero-vault-edge",
            "aero-im": "service:aero-im-edge",
        }
        for service, network_mode in expected_modes.items():
            self.assertEqual(
                self.compose["services"][service].get("network_mode"), network_mode
            )

        id_config = (PLATFORM_DIR / "config" / "aero-id.yaml").read_text()
        self.assertIn("endpoint: http://127.0.0.1:18080", id_config)
        self.assertIn("endpoint: http://127.0.0.1:18082", id_config)
        self.assertIn("endpoint: http://127.0.0.1:18081", id_config)
        self.assertIn("endpoint: http://127.0.0.1:18089", id_config)

        id_edge = (PLATFORM_DIR / "nginx" / "aero-id.conf").read_text()
        self.assertEqual(id_edge.count("location = /healthz"), 2)
        self.assertIn("proxy_pass http://$snaplink/readyz", id_edge)
        self.assertIn("proxy_pass http://$im/health/ready", id_edge)

    def test_snaplink_clients_pin_resources_and_scopes(self) -> None:
        config = (PLATFORM_DIR / "config" / "snaplink.yaml").read_text()
        contracts = {
            "aero-account-console": "allowed_resources: [aero-id]",
            "aero-id-snaplink-source": "allowed_resources: [snaplink-account-source]",
            "aero-id-im-source": "allowed_resources: [aero-im]",
            "aero-id-vault-source": "allowed_resources: [aero-vault]",
            "aero-im-vault": "allowed_scopes: [read, write]",
            "aero-id-audit": "allowed_scopes: [audit:event:write]",
            "aero-vault-audit": "allowed_scopes: [audit:event:write]",
            "platform-audit-bootstrap": "audit:platform:cross_tenant",
        }
        for client_id, contract in contracts.items():
            self.assertIn(f"id: {client_id}", config)
            self.assertIn(contract, config)

        public_client = config.split("id: aero-account-console", 1)[1].split(
            "id: aero-id-snaplink-source", 1
        )[0]
        self.assertIn("token_endpoint_auth_method: none", public_client)
        self.assertIn("require_pkce: true", public_client)
        self.assertIn("allowed_pkce_methods: [S256]", public_client)
        self.assertNotIn("client_secret:", public_client)

    def test_local_aero_im_account_summary_is_resource_aligned_but_fail_closed(self) -> None:
        id_config = (PLATFORM_DIR / "config" / "aero-id.yaml").read_text()
        aero_im_source = id_config.split("  aero_im:\n", 1)[1].split(
            "  aero_vault:\n", 1
        )[0]
        self.assertIn("oauth_resource: aero-im", aero_im_source)
        self.assertNotIn("target_assertion_", aero_im_source)

        im_environment = self.compose["services"]["aero-im"]["environment"]
        self.assertEqual(
            im_environment["AERO__INTEGRATIONS__AUDIENCE"], "aero-im-integration"
        )
        self.assertFalse(
            any(key.startswith("AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_") for key in im_environment)
        )

    def test_console_optional_commerce_routes_fail_explicitly(self) -> None:
        console = self.compose["services"]["snaplink-console"]
        self.assertEqual(
            console["environment"]["SNAPLINK_BILLING_UPSTREAM"],
            "http://optional-api-disabled:8088",
        )
        self.assertEqual(
            console["environment"]["SNAPLINK_STRIPE_ADAPTER_UPSTREAM"],
            "http://optional-api-disabled:8088",
        )

    def test_snaplink_hosted_login_uses_bundled_canvaskit(self) -> None:
        console_context = Path(
            self.compose["services"]["snaplink-console"]["build"]["context"]
        )
        bootstrap = (console_context / "web" / "flutter_bootstrap.js").read_text()
        self.assertIn("canvasKitBaseUrl: 'canvaskit/'", bootstrap)
        self.assertNotIn("gstatic.com", bootstrap)

    def test_aero_id_accepts_the_canonical_snaplink_issuer(self) -> None:
        config = (PLATFORM_DIR / "config" / "aero-id.yaml").read_text()
        account = config.split("account:", 1)[1].split("database:", 1)[0]
        self.assertIn("issuer: http://localhost:28080", account)

    def test_audit_source_algorithms_match_service_contracts(self) -> None:
        bootstrap = load_bootstrap_module()
        self.assertEqual(
            bootstrap.source_sha("aero-id", "platform-local"),
            "aero-id.sREwbx3C1xQgIrbXX0ipZbWFpSAPYmErLI-N9OckxYA",
        )
        self.assertEqual(
            bootstrap.vault_source(
                "platform-local", "Wf9Kp3Nq7Vc2Rm6Ty1Bh8Dz4Ls5Xj0Ea"
            ),
            "aero-vault.KicHGYifyesCeOGGc9AoPJv0LldkB9nvzl8THAr5lW8",
        )

    def test_vault_binding_is_fail_closed_and_not_group_writable(self) -> None:
        path = PLATFORM_DIR / "config" / "aero-vault-audit-bindings.json"
        value = json.loads(path.read_text())
        self.assertEqual(value["revision"], 1)
        self.assertEqual(
            value["bindings"],
            [
                {
                    "tenant_id": "platform-local",
                    "client_id": "aero-vault-audit",
                    "client_secret_env": "AUDIT_GOVERNANCE_CLIENT_SECRET_PLATFORM_LOCAL",
                    "state": "active",
                }
            ],
        )
        mode = stat.S_IMODE(os.stat(path).st_mode)
        self.assertEqual(mode & 0o022, 0)

    def test_ui_uses_snaplink_pkce_and_same_origin_aero_id_proxy(self) -> None:
        runtime = (PLATFORM_DIR / "config" / "runtime-config.js").read_text()
        snaplink = (PLATFORM_DIR / "config" / "snaplink.yaml").read_text()
        console = self.compose["services"]["aero-platform-console"]
        self.assertIn("snaplinkClientId: 'aero-account-console'", runtime)
        self.assertIn("snaplinkResource: 'aero-id'", runtime)
        self.assertIn("snaplinkHostedLoginUrl: 'http://localhost:28000/login/'", runtime)
        self.assertIn("aeroIdApiBase: 'http://localhost:28010/api/aero-id/v1'", runtime)
        self.assertIn("'audit:read'", runtime)
        self.assertIn("snaplinkConsoleUrl: 'http://localhost:28000/admin/'", runtime)
        public_client = snaplink.split("id: aero-account-console", 1)[1].split(
            "id: aero-im", 1
        )[0]
        self.assertIn("audit:read", public_client)
        self.assertEqual(
            console["healthcheck"]["test"],
            ["CMD-SHELL", "wget -qO- http://127.0.0.1/healthz >/dev/null"],
        )
        self.assertRegex(
            snaplink,
            r"allowed_origins:\n\s+- http://localhost:28000(?:\n|$)",
        )

    def test_aero_im_notification_projection_stays_content_free(self) -> None:
        integration = (
            PLATFORM_DIR.parents[1]
            / "crates"
            / "aero-server"
            / "src"
            / "integrations"
            / "account_summary.rs"
        ).read_text()
        aero_id_context = Path(self.compose["services"]["aero-id"]["build"]["context"])
        registry = (aero_id_context / "internal" / "domain" / "aggregation.go").read_text()
        ui_context = Path(
            self.compose["services"]["aero-platform-console"]["build"]["context"]
        )
        notification_page = (
            ui_context
            / "apps"
            / "aero-platform-console"
            / "src"
            / "pages"
            / "NotificationsPage.tsx"
        ).read_text()

        self.assertIn('"aero-im.notifications"', integration)
        self.assertIn('"notification_id": notification.id', integration)
        self.assertIn('"source_account_status": source_account_status', integration)
        self.assertNotIn('"content": notification.', integration)
        self.assertNotIn('"participant_id": notification.', integration)
        self.assertIn('"aero-im.notifications"', registry)
        self.assertIn("来自 Aero IM 权威收件箱的无正文投影", notification_page)
        self.assertIn("在 Aero IM 中处理", notification_page)

    def test_aero_vault_projection_stays_read_only_and_content_free(self) -> None:
        vault_context = Path(
            self.compose["services"]["aero-vault"]["build"]["context"]
        )
        summary = (
            vault_context / "internal" / "api" / "rest" / "account_summary.go"
        ).read_text()
        aero_id_context = Path(
            self.compose["services"]["aero-id"]["build"]["context"]
        )
        registry = (
            aero_id_context / "internal" / "domain" / "aggregation.go"
        ).read_text()
        ui_context = Path(
            self.compose["services"]["aero-platform-console"]["build"]["context"]
        )
        storage_page = (
            ui_context
            / "apps"
            / "aero-platform-console"
            / "src"
            / "pages"
            / "StoragePage.tsx"
        ).read_text()

        for dataset in (
            "aero-vault.tenants",
            "aero-vault.buckets",
            "aero-vault.usage",
        ):
            self.assertIn(f'"{dataset}"', summary)
            self.assertIn(f'"{dataset}"', registry)
        self.assertNotIn('"content"', summary)
        self.assertNotIn('"download_url"', summary)
        self.assertIn("来自 Aero Vault 的只读租户、桶和用量投影", storage_page)
        self.assertIn("在 Aero Vault 中管理", storage_page)
        self.assertIn("不会把 Aero ID access token", storage_page)
        self.assertNotIn("fetch(", storage_page)

    def test_snaplink_security_projection_is_allow_listed_and_read_only(self) -> None:
        snaplink_context = Path(
            self.compose["services"]["snaplink"]["build"]["context"]
        )
        summary = (
            snaplink_context / "cmd" / "sso-server" / "serveraccount" / "account_summary.go"
        ).read_text()
        aero_id_context = Path(
            self.compose["services"]["aero-id"]["build"]["context"]
        )
        registry = (
            aero_id_context / "internal" / "domain" / "aggregation.go"
        ).read_text()
        ui_context = Path(
            self.compose["services"]["aero-platform-console"]["build"]["context"]
        )
        security_page = (
            ui_context
            / "apps"
            / "aero-platform-console"
            / "src"
            / "pages"
            / "SecurityPage.tsx"
        ).read_text()
        security_projection = (
            ui_context
            / "apps"
            / "aero-platform-console"
            / "src"
            / "pages"
            / "identitySecurityProjection.ts"
        ).read_text()

        for dataset in (
            "snaplink.identity",
            "snaplink.tenants",
            "snaplink.permissions",
            "snaplink.security",
        ):
            self.assertIn(f'"{dataset}"', summary)
            self.assertIn(f'"{dataset}"', registry)
        self.assertNotIn('"password":', summary)
        self.assertNotIn('"mfa_secret":', summary)
        self.assertNotIn('"session_token":', summary)
        self.assertIn("认证权威始终是 Snaplink", security_page)
        self.assertIn("在 Snaplink 中管理", security_page)
        self.assertIn("不会显示密钥、令牌或", security_page)
        self.assertIn("snapshotMetadata", security_projection)
        self.assertNotIn("fetch(", security_page)

    def test_snaplink_existing_volume_sync_preserves_client_shape(self) -> None:
        sync = load_snaplink_sync_module()
        current = {
            "id": "aero-account-console",
            "name": "Aero Account Console",
            "redirectUris": ["http://localhost:28010/"],
            "allowedScopes": ["openid", "profile"],
            "allowedAuthenticators": ["password"],
            "tokenStrategy": "jwt",
            "active": True,
            "tenantId": "platform-local",
            "grantTypes": ["authorization_code"],
        }
        desired = {
            "name": "Aero Account Console",
            "redirectUris": ["http://localhost:28010/"],
            "loginPageUri": "",
            "allowedScopes": ["openid", "profile", "audit:read"],
            "allowedAuthenticators": ["password"],
            "tokenStrategy": "jwt",
            "active": True,
        }
        update = sync.project_update("aero-account-console", current, desired)
        self.assertEqual(update["allowedScopes"], desired["allowedScopes"])
        self.assertEqual(update["redirectUris"], current["redirectUris"])
        self.assertEqual(update["allowedAuthenticators"], ["password"])
        self.assertNotIn("tenantId", update)
        self.assertNotIn("grantTypes", update)

        service = self.compose["services"]["snaplink-config-sync"]
        self.assertEqual(service["restart"], "no")
        self.assertEqual(
            service["environment"]["SNAPLINK_MANAGED_CLIENT_ID"],
            "aero-account-console",
        )

    def test_snaplink_roles_cover_console_admin_and_aero_id_authorizer(self) -> None:
        config = (PLATFORM_DIR / "config" / "snaplink.yaml").read_text()
        self.assertIn("client_id: sso-admin-console", config)
        self.assertIn("permissions: [admin:*]", config)
        self.assertIn("client_id: aero-id", config)
        self.assertIn("code: aero-platform-admin", config)
        for permission in (
            "account.read",
            "account.manage",
            "operation.read",
            "operation.manage",
            "source.sync",
            "account.export",
            "account.erase",
            "audit.read",
        ):
            self.assertIn(f"- {permission}", config)


if __name__ == "__main__":
    unittest.main()
