import { expect, test, token } from './helpers.ts';

// Signs in once through the login page; the other specs reuse the cookie.
test('sign in', async ({ page }) => {
  await page.goto('/');
  await expect(page).toHaveURL(/\/login\?next=%2F$/);
  await page.getByLabel('Access token').fill(token());
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page).toHaveURL(/\/$/);
  await expect(page.getByTestId('search-input')).toBeVisible();
  await page.context().storageState({ path: 'e2e/.auth/state.json' });
});
