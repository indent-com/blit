import { test, expect } from '@playwright/test';

test.describe('Auth flow', () => {
  test.beforeEach(async ({ page }) => {
    // Clear any saved passphrase
    await page.goto('/');
    await page.evaluate(() => localStorage.clear());
  });

  test('page loads and shows passphrase input', async ({ page }) => {
    await page.goto('/');
    const authEl = page.locator('#auth');
    await expect(authEl).toBeVisible();
    const passInput = page.locator('#pass');
    await expect(passInput).toBeVisible();
    await expect(passInput).toHaveAttribute('type', 'password');
  });

  test('wrong passphrase shows "auth failed" and input is re-usable', async ({ page }) => {
    await page.goto('/');
    const passInput = page.locator('#pass');
    await expect(passInput).toBeVisible();

    // Submit a wrong passphrase
    await passInput.fill('wrong-password');
    await passInput.press('Enter');

    // Should show auth failed message
    const status = page.locator('#status');
    await expect(status).toContainText('auth failed', { timeout: 10_000 });

    // The auth form should still be visible
    const authEl = page.locator('#auth');
    await expect(authEl).toBeVisible();

    // The input should be re-focusable and accept new input (this is the bug fix)
    await expect(passInput).not.toBeDisabled();
    await passInput.fill('another-attempt');
    await expect(passInput).toHaveValue('another-attempt');
  });

  test('correct passphrase hides auth form and shows workspace', async ({ page }) => {
    await page.goto('/');
    const passInput = page.locator('#pass');
    await expect(passInput).toBeVisible();

    await passInput.fill('test-secret');
    await passInput.press('Enter');

    // Auth should disappear and workspace should appear
    const authEl = page.locator('#auth');
    await expect(authEl).toBeHidden({ timeout: 10_000 });

    const workspace = page.locator('#workspace');
    await expect(workspace).toBeVisible({ timeout: 10_000 });
  });

  test('saved passphrase auto-connects on reload', async ({ page }) => {
    await page.goto('/');
    const passInput = page.locator('#pass');
    await expect(passInput).toBeVisible();

    // First, authenticate successfully
    await passInput.fill('test-secret');
    await passInput.press('Enter');

    const workspace = page.locator('#workspace');
    await expect(workspace).toBeVisible({ timeout: 10_000 });

    // Reload the page - should auto-connect with saved passphrase
    await page.reload();

    // Should go straight to workspace without showing auth
    await expect(workspace).toBeVisible({ timeout: 10_000 });
  });

  test('invalid saved passphrase shows "saved passphrase failed" and input is usable', async ({ page }) => {
    // Set a bad passphrase in localStorage before loading
    await page.goto('/');
    await page.evaluate(() => {
      localStorage.setItem('blit.passphrase', 'bad-saved-pass');
    });

    // Reload so the app reads the saved passphrase
    await page.reload();

    // Should show "saved passphrase failed"
    const status = page.locator('#status');
    await expect(status).toContainText('saved passphrase failed', { timeout: 10_000 });

    // Auth should be visible
    const authEl = page.locator('#auth');
    await expect(authEl).toBeVisible();

    // The password input should be usable - verify keystrokes go into #pass, NOT #input-sink
    const passInput = page.locator('#pass');
    await expect(passInput).not.toBeDisabled();

    // Focus the password input explicitly and type
    await passInput.focus();
    await page.keyboard.type('test-secret');

    // Verify the typed text went into the password field
    await expect(passInput).toHaveValue('test-secret');

    // Verify text did NOT go to the input sink (the terminal textarea)
    const inputSinkValue = await page.locator('#input-sink').inputValue();
    expect(inputSinkValue).toBe('');

    // Now submit the correct passphrase and verify it works
    await passInput.press('Enter');
    await expect(authEl).toBeHidden({ timeout: 10_000 });
    const workspace = page.locator('#workspace');
    await expect(workspace).toBeVisible({ timeout: 10_000 });
  });
});
