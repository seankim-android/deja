const screens = ['screen_login', 'screen_catalog', 'screen_cart', 'order_confirmation']

function showScreen(id) {
  for (const screen of screens) document.getElementById(screen).hidden = screen !== id
  document.getElementById('login_error').hidden = true
}

function bindFixture() {
  document.getElementById('login_button').addEventListener('click', () => {
    const email = document.getElementById('username').value
    const password = document.getElementById('password').value
    if (email === 'qa@example.com' && password === 'testing123') showScreen('screen_catalog')
    else document.getElementById('login_error').hidden = false
  })
  document.getElementById('add_to_cart').addEventListener('click', () => showScreen('screen_cart'))
  document.getElementById('checkout_button').addEventListener('click', async () => {
    const response = await fetch('/api/order')
    if (!response.ok) throw new Error('Fixture order service unavailable')
    const order = await response.json()
    document.getElementById('order_number').textContent =
      `Order ${order.orderNumber} · Mochi sticker pack`
    showScreen('order_confirmation')
  })
}

bindFixture()
