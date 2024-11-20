defmodule FloeWeb.PageController do
  use FloeWeb, :controller

  def home(conn, _params) do
    render(conn, :home)
  end
end
